//! mTLS inbound listener: TLS termination, HTTP parsing, traceparent injection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use super::metrics::RedMetrics;
use super::tls::{self, SharedCerts};
use super::traces::{self, ActiveSpan, SharedActiveSpans, SpanRecord};

/// Parameters for the inbound listener to avoid too-many-arguments.
pub struct InboundParams {
    pub tls_port: u16,
    pub app_port: u16,
    pub service_name: String,
    pub certs: SharedCerts,
    pub span_tx: mpsc::Sender<SpanRecord>,
    pub active_spans: SharedActiveSpans,
    pub red_metrics: Arc<RedMetrics>,
}

/// Run the mTLS inbound listener.
///
/// Listens on `PROXY_TLS_PORT` (default 8443), terminates mTLS, parses HTTP,
/// injects traceparent, forwards to `localhost:app_port`, and generates SERVER spans.
#[tracing::instrument(skip_all, fields(tls_port = params.tls_port, app_port = params.app_port))]
pub async fn run_inbound_listener(params: InboundParams, mut shutdown: watch::Receiver<()>) {
    let addr = SocketAddr::from(([0, 0, 0, 0], params.tls_port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to bind inbound TLS listener");
            return;
        }
    };
    tracing::info!(port = params.tls_port, "inbound mTLS listener started");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        let certs = params.certs.clone();
                        let span_tx = params.span_tx.clone();
                        let active_spans = params.active_spans.clone();
                        let app_port = params.app_port;
                        let service = params.service_name.clone();
                        let red = params.red_metrics.clone();

                        tokio::spawn(Box::pin(async move {
                            if let Err(e) = handle_inbound_connection(
                                stream, peer, certs, span_tx, active_spans,
                                app_port, &service, red,
                            ).await {
                                tracing::debug!(
                                    error = %e,
                                    peer = %peer,
                                    "inbound connection error"
                                );
                            }
                        }));
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "inbound accept error");
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("inbound listener exiting");
}

/// Resolve trace context from traceparent header or generate new.
fn resolve_trace_context(traceparent: Option<&str>) -> (String, Option<String>) {
    traceparent
        .and_then(|tp| traces::parse_traceparent(tp).map(|(tid, psid, _)| (tid, Some(psid))))
        .unwrap_or_else(|| (traces::new_trace_id(), None))
}

/// Forward a request to the local app and read the response.
async fn forward_to_app(request: &[u8], app_port: u16) -> anyhow::Result<(Vec<u8>, u16)> {
    let mut upstream = TcpStream::connect(format!("127.0.0.1:{app_port}"))
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to app on port {app_port}: {e}"))?;
    upstream.write_all(request).await?;

    let mut response_buf = Vec::with_capacity(16384);
    let mut temp = vec![0u8; 8192];
    match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        read_http_response(&mut upstream, &mut response_buf, &mut temp),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(error = %e, "upstream read error"),
        Err(_) => tracing::warn!("upstream response timeout"),
    }
    let status_code = parse_http_status(&response_buf);
    Ok((response_buf, status_code))
}

/// Handle a single inbound mTLS connection.
#[allow(clippy::too_many_arguments)]
async fn handle_inbound_connection(
    stream: TcpStream,
    peer: SocketAddr,
    certs: SharedCerts,
    span_tx: mpsc::Sender<SpanRecord>,
    active_spans: SharedActiveSpans,
    app_port: u16,
    service: &str,
    red_metrics: Arc<RedMetrics>,
) -> anyhow::Result<()> {
    let current_certs = certs.load();
    let acceptor = tls::build_tls_acceptor(&current_certs)?;
    let mut tls_stream = acceptor
        .accept(stream)
        .await
        .map_err(|e| anyhow::anyhow!("TLS handshake failed from {peer}: {e}"))?;

    let caller_spiffe = {
        let (_, server_conn) = tls_stream.get_ref();
        server_conn
            .peer_certificates()
            .and_then(|c| c.first())
            .and_then(|cert| tls::extract_spiffe_id(cert.as_ref()))
    };

    let mut request_buf = vec![0u8; 16384];
    let n = tls_stream.read(&mut request_buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request_bytes = &request_buf[..n];
    let request_str = String::from_utf8_lossy(request_bytes);
    let (method, path, traceparent_header) = parse_http_request(&request_str);
    let start_time = Instant::now();
    let started_at = Utc::now();
    let (trace_id, parent_span_id) = resolve_trace_context(traceparent_header.as_deref());
    let span_id = traces::new_span_id();

    active_spans.write().await.insert(
        span_id.clone(),
        ActiveSpan {
            trace_id: trace_id.clone(),
            span_id: span_id.clone(),
            started_at: start_time,
        },
    );

    let traceparent = traces::build_traceparent(&trace_id, &span_id);
    let forwarded = inject_traceparent(request_bytes, &traceparent);
    let (response_buf, status_code) = forward_to_app(&forwarded, app_port).await?;

    if response_buf.is_empty() {
        tls_stream
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await?;
    } else {
        tls_stream.write_all(&response_buf).await?;
    }
    tls_stream.shutdown().await?;

    active_spans.write().await.remove(&span_id);

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;
    red_metrics.record(duration_ms, status_code >= 500);

    let span_name = format!("{method} {path}");
    let mut extra_attrs = vec![
        ("http.method".into(), method),
        ("http.url".into(), path),
        ("net.peer.ip".into(), peer.ip().to_string()),
    ];
    if let Some(ref spiffe) = caller_spiffe {
        extra_attrs.push(("mesh.caller.spiffe_id".into(), spiffe.clone()));
    }
    let span = traces::build_server_span(
        &trace_id,
        &span_id,
        parent_span_id.as_deref(),
        &span_name,
        service,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        status_code,
        extra_attrs,
    );
    let _ = span_tx.try_send(span);
    Ok(())
}

/// Read the full HTTP response from the upstream.
async fn read_http_response(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    temp: &mut [u8],
) -> anyhow::Result<()> {
    loop {
        let n = stream.read(temp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[..n]);
        // Simple heuristic: if we've read the headers and body based on
        // Content-Length, we can stop. For simplicity, just read until close.
        if has_complete_response(buf) {
            break;
        }
    }
    Ok(())
}

/// Check if we have a complete HTTP response (headers + body per Content-Length).
fn has_complete_response(data: &[u8]) -> bool {
    let header_end = find_header_end(data);
    if header_end == 0 {
        return false;
    }
    // Check for Content-Length
    let headers = String::from_utf8_lossy(&data[..header_end]);
    if let Some(cl) = extract_content_length(&headers) {
        let body_start = header_end;
        let body_len = data.len() - body_start;
        body_len >= cl
    } else {
        // No Content-Length — check for chunked or just return true
        // (connection will close when done)
        false
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

/// Parse the HTTP request line to extract method and path, plus traceparent header.
fn parse_http_request(request: &str) -> (String, String, Option<String>) {
    let mut method = String::new();
    let mut path = String::new();
    let mut traceparent = None;

    for (i, line) in request.lines().enumerate() {
        if i == 0 {
            // Request line: "METHOD PATH HTTP/1.1"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                method = parts[0].to_string();
                path = parts[1].to_string();
            }
        } else if line.is_empty() {
            break;
        } else {
            let lower = line.to_lowercase();
            if lower.starts_with("traceparent:") {
                traceparent = line.split_once(':').map(|(_, v)| v.trim().to_string());
            }
        }
    }

    (method, path, traceparent)
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

/// Inject or replace traceparent header in the HTTP request bytes.
fn inject_traceparent(request: &[u8], traceparent: &str) -> Vec<u8> {
    let req_str = String::from_utf8_lossy(request);

    // Find the end of the first line
    let mut result = String::with_capacity(req_str.len() + 80);
    let mut found_traceparent = false;
    let mut in_body = false;

    for line in req_str.split("\r\n") {
        if in_body {
            result.push_str(line);
            result.push_str("\r\n");
            continue;
        }
        if line.is_empty() {
            // End of headers
            if !found_traceparent {
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
            found_traceparent = true;
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
    fn parse_http_request_basic() {
        let request = "GET /api/test HTTP/1.1\r\nHost: localhost\r\ntraceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\r\n\r\n";
        let (method, path, tp) = parse_http_request(request);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/test");
        assert_eq!(
            tp,
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into())
        );
    }

    #[test]
    fn parse_http_request_no_traceparent() {
        let request = "POST /upload HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (method, path, tp) = parse_http_request(request);
        assert_eq!(method, "POST");
        assert_eq!(path, "/upload");
        assert!(tp.is_none());
    }

    #[test]
    fn parse_http_status_200() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        assert_eq!(parse_http_status(response), 200);
    }

    #[test]
    fn parse_http_status_404() {
        let response = b"HTTP/1.1 404 Not Found\r\n\r\n";
        assert_eq!(parse_http_status(response), 404);
    }

    #[test]
    fn parse_http_status_empty() {
        assert_eq!(parse_http_status(b""), 502);
    }

    #[test]
    fn inject_traceparent_new() {
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let result = inject_traceparent(request, "00-trace-span-01");
        let s = String::from_utf8(result).unwrap();
        assert!(s.contains("traceparent: 00-trace-span-01"));
    }

    #[test]
    fn inject_traceparent_replace() {
        let request = b"GET / HTTP/1.1\r\ntraceparent: old-value\r\nHost: localhost\r\n\r\n";
        let result = inject_traceparent(request, "00-new-value-01");
        let s = String::from_utf8(result).unwrap();
        assert!(s.contains("traceparent: 00-new-value-01"));
        assert!(!s.contains("old-value"));
    }

    #[test]
    fn find_header_end_basic() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(find_header_end(data), data.len());
    }

    #[test]
    fn find_header_end_missing() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n";
        assert_eq!(find_header_end(data), 0);
    }

    #[test]
    fn extract_content_length_present() {
        let headers = "HTTP/1.1 200 OK\r\nContent-Length: 42\r\n";
        assert_eq!(extract_content_length(headers), Some(42));
    }

    #[test]
    fn extract_content_length_missing() {
        let headers = "HTTP/1.1 200 OK\r\n";
        assert_eq!(extract_content_length(headers), None);
    }

    #[test]
    fn has_complete_response_with_body() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        assert!(has_complete_response(data));
    }

    #[test]
    fn has_complete_response_incomplete() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nok";
        assert!(!has_complete_response(data));
    }
}
