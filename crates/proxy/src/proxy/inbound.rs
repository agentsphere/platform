//! mTLS inbound listener: TLS termination, HTTP parsing, traceparent injection.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use super::config::MtlsMode;
use super::metrics::RedMetrics;
use super::tls::{self, SharedCerts};
use super::traces::{self, ActiveSpan, SharedActiveSpans, SpanRecord};
use super::transparent;

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

// ---------------------------------------------------------------------------
// Transparent inbound proxy
// ---------------------------------------------------------------------------

/// Parameters for the transparent inbound proxy.
pub struct TransparentInboundParams {
    pub inbound_port: u16,
    pub mtls_mode: MtlsMode,
    pub node_cidrs: Vec<(IpAddr, u8)>,
    pub bypass_port_range: (u16, u16),
    pub passthrough_ports: Vec<u16>,
    pub service_name: String,
    pub certs: SharedCerts,
    pub span_tx: mpsc::Sender<SpanRecord>,
    pub active_spans: SharedActiveSpans,
    pub red_metrics: Arc<RedMetrics>,
}

/// Run the transparent inbound listener.
///
/// Binds `0.0.0.0:{inbound_port}` (default 15006). For each accepted connection:
/// 1. Recover original destination via `SO_ORIGINAL_DST`.
/// 2. Peek for TLS `ClientHello` (0x16) -- if present, do TLS handshake.
/// 3. Peek for HTTP method -- if HTTP, inject traceparent and generate SERVER span.
/// 4. Forward to `original_ip:original_port` via a socket bound to `outbound_bind_addr`.
#[tracing::instrument(skip_all, fields(port = params.inbound_port))]
pub async fn run_transparent_inbound(
    params: TransparentInboundParams,
    mut shutdown: watch::Receiver<()>,
) {
    let addr = SocketAddr::from(([0, 0, 0, 0], params.inbound_port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to bind transparent inbound listener");
            return;
        }
    };
    tracing::info!(
        port = params.inbound_port,
        "transparent inbound listener started"
    );

    let params = Arc::new(params);
    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        let p = params.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_transparent_inbound(stream, peer, &p).await {
                                tracing::debug!(error = %e, peer = %peer, "transparent inbound error");
                            }
                        });
                    }
                    Err(e) => tracing::debug!(error = %e, "transparent inbound accept error"),
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("transparent inbound listener exiting");
}

/// Top-level handler for one transparent inbound connection.
async fn handle_transparent_inbound(
    stream: TcpStream,
    peer: SocketAddr,
    params: &TransparentInboundParams,
) -> anyhow::Result<()> {
    let original_dst = transparent::get_original_dst(&stream)
        .await
        .map_err(|e| anyhow::anyhow!("get_original_dst failed: {e}"))?;

    // TCP passthrough for known non-HTTP ports — skip TLS and strict mode
    if params.passthrough_ports.contains(&original_dst.port()) {
        let (read, write) = stream.into_split();
        return forward_transparent_inbound(read, write, peer, original_dst, None, params).await;
    }

    if transparent::is_tls_client_hello(&stream).await {
        handle_tls_inbound(stream, peer, original_dst, params).await
    } else {
        handle_plain_inbound(stream, peer, original_dst, params).await
    }
}

/// Handle a TLS connection on the transparent inbound path.
async fn handle_tls_inbound(
    stream: TcpStream,
    peer: SocketAddr,
    original_dst: SocketAddr,
    params: &TransparentInboundParams,
) -> anyhow::Result<()> {
    let current_certs = params.certs.load();
    let acceptor = tls::build_permissive_tls_acceptor(&current_certs)?;
    let tls_stream = acceptor
        .accept(stream)
        .await
        .map_err(|e| anyhow::anyhow!("transparent TLS handshake failed from {peer}: {e}"))?;

    let caller_spiffe = {
        let (_, server_conn) = tls_stream.get_ref();
        server_conn
            .peer_certificates()
            .and_then(|c| c.first())
            .and_then(|cert| tls::extract_spiffe_id(cert.as_ref()))
    };

    let (tls_read, tls_write) = tokio::io::split(tls_stream);
    forward_transparent_inbound(
        tls_read,
        tls_write,
        peer,
        original_dst,
        caller_spiffe,
        params,
    )
    .await
}

/// Handle a plaintext connection on the transparent inbound path.
async fn handle_plain_inbound(
    stream: TcpStream,
    peer: SocketAddr,
    original_dst: SocketAddr,
    params: &TransparentInboundParams,
) -> anyhow::Result<()> {
    // In strict mode, only allow plaintext from node CIDRs (kubelets).
    if params.mtls_mode == MtlsMode::Strict
        && !transparent::is_internal_ip(peer.ip(), &params.node_cidrs)
    {
        tracing::debug!(
            peer = %peer,
            "strict mode: rejecting plaintext from non-node IP"
        );
        return Ok(());
    }

    let (read_half, write_half) = stream.into_split();
    forward_transparent_inbound(read_half, write_half, peer, original_dst, None, params).await
}

/// Forward an inbound connection (TLS-terminated or plain) to the original destination.
///
/// Peeks to detect HTTP vs TCP. HTTP gets traceparent injection + SERVER span.
/// TCP gets a CONNECTION span with bidirectional copy.
#[allow(clippy::too_many_arguments)]
async fn forward_transparent_inbound<R, W>(
    mut reader: R,
    writer: W,
    peer: SocketAddr,
    original_dst: SocketAddr,
    caller_spiffe: Option<String>,
    params: &TransparentInboundParams,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Connect to the original destination using a bypass source port to
    // prevent iptables from re-intercepting the packet.
    let mut upstream =
        transparent::bind_outbound_socket(original_dst, params.bypass_port_range).await?;

    // Read first chunk to detect protocol
    let mut buf = vec![0u8; 16384];
    let n = reader.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let first_bytes = &buf[..n];

    if transparent::detect_http_prefix_pub(first_bytes) {
        forward_http_inbound(
            first_bytes,
            reader,
            writer,
            &mut upstream,
            peer,
            caller_spiffe,
            params,
        )
        .await
    } else {
        forward_tcp_inbound(first_bytes, reader, writer, upstream, peer, params).await
    }
}

/// Forward an HTTP request through the transparent inbound path.
#[allow(clippy::too_many_arguments)]
async fn forward_http_inbound<R, W>(
    first_bytes: &[u8],
    _reader: R,
    mut writer: W,
    upstream: &mut TcpStream,
    peer: SocketAddr,
    caller_spiffe: Option<String>,
    params: &TransparentInboundParams,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let request_str = String::from_utf8_lossy(first_bytes);
    let (method, path, traceparent_header) = parse_http_request(&request_str);
    let start_time = Instant::now();
    let started_at = Utc::now();
    let (trace_id, parent_span_id) = resolve_trace_context(traceparent_header.as_deref());
    let span_id = traces::new_span_id();

    params.active_spans.write().await.insert(
        span_id.clone(),
        ActiveSpan {
            trace_id: trace_id.clone(),
            span_id: span_id.clone(),
            started_at: start_time,
        },
    );

    let traceparent = traces::build_traceparent(&trace_id, &span_id);
    let forwarded = inject_traceparent(first_bytes, &traceparent);
    upstream.write_all(&forwarded).await?;

    // Read response
    let mut response_buf = Vec::with_capacity(16384);
    let mut temp = vec![0u8; 8192];
    match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        read_http_response(upstream, &mut response_buf, &mut temp),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(error = %e, "transparent upstream read error"),
        Err(_) => tracing::warn!("transparent upstream response timeout"),
    }
    let status_code = parse_http_status(&response_buf);

    if response_buf.is_empty() {
        writer
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await?;
    } else {
        writer.write_all(&response_buf).await?;
    }

    params.active_spans.write().await.remove(&span_id);

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;
    params.red_metrics.record(duration_ms, status_code >= 500);

    let span_name = format!("{method} {path}");
    let mut extra_attrs = vec![
        ("http.method".into(), method),
        ("http.url".into(), path),
        ("net.peer.ip".into(), peer.ip().to_string()),
        ("mesh.transparent".into(), "true".into()),
    ];
    if let Some(ref spiffe) = caller_spiffe {
        extra_attrs.push(("mesh.caller.spiffe_id".into(), spiffe.clone()));
    }
    let span = traces::build_server_span(
        &trace_id,
        &span_id,
        parent_span_id.as_deref(),
        &span_name,
        &params.service_name,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        status_code,
        extra_attrs,
    );
    let _ = params.span_tx.try_send(span);
    Ok(())
}

/// Forward a TCP stream through the transparent inbound path (bidirectional copy).
async fn forward_tcp_inbound<R, W>(
    first_bytes: &[u8],
    reader: R,
    client_writer: W,
    upstream: TcpStream,
    peer: SocketAddr,
    params: &TransparentInboundParams,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let started_at = Utc::now();
    let start_time = Instant::now();
    let trace_id = traces::new_trace_id();
    let span_id = traces::new_span_id();

    let (upstream_read, mut upstream_write) = tokio::io::split(upstream);

    // Write the bytes we already read to upstream
    upstream_write.write_all(first_bytes).await?;

    let bytes_in = Arc::new(std::sync::atomic::AtomicU64::new(first_bytes.len() as u64));
    let bytes_out = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let bi = bytes_in.clone();
    let copy_in = tokio::spawn(async move { counted_copy(reader, upstream_write, bi).await });
    let bo = bytes_out.clone();
    let copy_out =
        tokio::spawn(async move { counted_copy(upstream_read, client_writer, bo).await });

    let _ = tokio::try_join!(copy_in, copy_out);

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;
    let total_bytes = bytes_in.load(std::sync::atomic::Ordering::Relaxed)
        + bytes_out.load(std::sync::atomic::Ordering::Relaxed);

    let span = traces::build_connection_span(
        &trace_id,
        &span_id,
        &params.service_name,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        total_bytes,
    );
    let _ = params.span_tx.try_send(span);

    tracing::debug!(
        peer = %peer,
        bytes = total_bytes,
        duration_ms,
        "transparent inbound TCP session ended"
    );
    Ok(())
}

/// Copy bytes from reader to writer, counting bytes transferred.
async fn counted_copy<R, W>(
    mut reader: R,
    mut writer: W,
    counter: Arc<std::sync::atomic::AtomicU64>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        counter.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        writer.write_all(&buf[..n]).await?;
    }
    writer.shutdown().await?;
    Ok(())
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
