//! mTLS outbound proxy: apps connect here, proxy originates mTLS to upstream.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::Utc;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use super::tls::{self, SharedCerts};
use super::traces::{self, SpanRecord};
use super::transparent;

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

// ---------------------------------------------------------------------------
// Transparent outbound proxy
// ---------------------------------------------------------------------------

/// Parameters for the transparent outbound proxy.
pub struct TransparentOutboundParams {
    pub outbound_port: u16,
    pub bypass_port_range: (u16, u16),
    pub internal_cidrs: Vec<(IpAddr, u8)>,
    pub passthrough_ports: Vec<u16>,
    pub service_name: String,
    pub certs: SharedCerts,
    pub span_tx: mpsc::Sender<SpanRecord>,
}

/// Run the transparent outbound proxy.
///
/// Binds `0.0.0.0:{outbound_port}` (default 15001). For each connection:
/// 1. Recover original destination via `SO_ORIGINAL_DST`.
/// 2. If dest is internal (matches `internal_cidrs`): originate mTLS.
/// 3. If dest is external: raw TCP passthrough.
#[tracing::instrument(skip_all, fields(port = params.outbound_port))]
pub async fn run_transparent_outbound(
    params: TransparentOutboundParams,
    mut shutdown: watch::Receiver<()>,
) {
    let addr = SocketAddr::from(([0, 0, 0, 0], params.outbound_port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to bind transparent outbound proxy");
            return;
        }
    };
    tracing::info!(
        port = params.outbound_port,
        "transparent outbound proxy started"
    );

    let params = Arc::new(params);
    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _peer)) => {
                        let p = params.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_transparent_outbound(stream, &p).await {
                                tracing::debug!(error = %e, "transparent outbound error");
                            }
                        });
                    }
                    Err(e) => tracing::debug!(error = %e, "transparent outbound accept error"),
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("transparent outbound proxy exiting");
}

/// Top-level handler for one transparent outbound connection.
async fn handle_transparent_outbound(
    app_stream: TcpStream,
    params: &TransparentOutboundParams,
) -> anyhow::Result<()> {
    let original_dst = transparent::get_original_dst(&app_stream)
        .await
        .map_err(|e| anyhow::anyhow!("outbound get_original_dst failed: {e}"))?;

    // TCP passthrough for known non-HTTP ports (Postgres, Redis, etc.)
    if params.passthrough_ports.contains(&original_dst.port()) {
        let internal = transparent::is_internal_ip(original_dst.ip(), &params.internal_cidrs);
        return handle_outbound_passthrough(app_stream, original_dst, params, internal).await;
    }

    if transparent::is_internal_ip(original_dst.ip(), &params.internal_cidrs) {
        handle_outbound_internal(app_stream, original_dst, params).await
    } else {
        handle_outbound_passthrough(app_stream, original_dst, params, false).await
    }
}

/// Outbound to an internal service: originate mTLS, sniff HTTP for tracing.
async fn handle_outbound_internal(
    mut app_stream: TcpStream,
    dest: SocketAddr,
    params: &TransparentOutboundParams,
) -> anyhow::Result<()> {
    // Read first chunk from the app
    let mut buf = vec![0u8; 16384];
    let n = app_stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let first_bytes = &buf[..n];

    // Establish mTLS to the destination (connect to same port -- the
    // destination's inbound transparent proxy will terminate TLS).
    let upstream_tcp = transparent::bind_outbound_socket(dest, params.bypass_port_range).await?;

    let current_certs = params.certs.load();
    let connector = tls::build_tls_connector(&current_certs)?;
    let dest_host = dest.ip().to_string();
    let server_name = ServerName::try_from(dest_host.clone())
        .map_err(|e| anyhow::anyhow!("invalid server name '{dest_host}': {e}"))?;
    let mut tls_stream = connector
        .connect(server_name, upstream_tcp)
        .await
        .map_err(|e| anyhow::anyhow!("outbound mTLS to {dest} failed: {e}"))?;

    if transparent::detect_http_prefix_pub(first_bytes) {
        outbound_http_internal(first_bytes, &mut app_stream, &mut tls_stream, params).await
    } else {
        outbound_tcp_internal(first_bytes, app_stream, tls_stream, params).await
    }
}

/// Forward an HTTP request to an internal mTLS peer, with tracing.
async fn outbound_http_internal(
    first_bytes: &[u8],
    app_stream: &mut TcpStream,
    tls_stream: &mut tokio_rustls::client::TlsStream<TcpStream>,
    params: &TransparentOutboundParams,
) -> anyhow::Result<()> {
    let request_str = String::from_utf8_lossy(first_bytes);
    let (method, path, _host, traceparent) = parse_outbound_request(&request_str);

    let start_time = Instant::now();
    let started_at = Utc::now();

    let (trace_id, parent_span_id) = traceparent
        .as_deref()
        .and_then(|tp| traces::parse_traceparent(tp).map(|(tid, psid, _)| (tid, Some(psid))))
        .unwrap_or_else(|| (traces::new_trace_id(), None));
    let span_id = traces::new_span_id();

    let tp_header = traces::build_traceparent(&trace_id, &span_id);
    let forwarded = inject_outbound_traceparent(first_bytes, &tp_header);
    tls_stream.write_all(&forwarded).await?;

    // Read response
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

    let span_name = format!("{method} {path}");
    let span = traces::build_client_span(
        &trace_id,
        &span_id,
        parent_span_id.as_deref(),
        &span_name,
        &params.service_name,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        status_code,
    );
    let _ = params.span_tx.try_send(span);
    Ok(())
}

/// Forward a TCP stream to an internal mTLS peer (bidirectional copy).
async fn outbound_tcp_internal(
    first_bytes: &[u8],
    app_stream: TcpStream,
    tls_stream: tokio_rustls::client::TlsStream<TcpStream>,
    params: &TransparentOutboundParams,
) -> anyhow::Result<()> {
    let started_at = Utc::now();
    let start_time = Instant::now();
    let trace_id = traces::new_trace_id();
    let span_id = traces::new_span_id();

    let (tls_read, mut tls_write) = tokio::io::split(tls_stream);
    let (app_read, app_write) = app_stream.into_split();

    // Write already-read bytes
    tls_write.write_all(first_bytes).await?;

    let bytes_out = Arc::new(AtomicU64::new(first_bytes.len() as u64));
    let bytes_in = Arc::new(AtomicU64::new(0));

    let bo = bytes_out.clone();
    let copy_out =
        tokio::spawn(async move { outbound_counted_copy(app_read, tls_write, bo).await });
    let bi = bytes_in.clone();
    let copy_in = tokio::spawn(async move { outbound_counted_copy(tls_read, app_write, bi).await });

    let _ = tokio::try_join!(copy_out, copy_in);

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;
    let total = bytes_in.load(Ordering::Relaxed) + bytes_out.load(Ordering::Relaxed);

    let span = traces::build_connection_span(
        &trace_id,
        &span_id,
        &params.service_name,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        total,
    );
    let _ = params.span_tx.try_send(span);
    Ok(())
}

/// Raw TCP passthrough — used for external destinations and internal passthrough
/// ports (Postgres, Redis). The `internal` flag controls span tagging so
/// observability dashboards correctly classify the traffic.
async fn handle_outbound_passthrough(
    app_stream: TcpStream,
    dest: SocketAddr,
    params: &TransparentOutboundParams,
    internal: bool,
) -> anyhow::Result<()> {
    let upstream = transparent::bind_outbound_socket(dest, params.bypass_port_range).await?;

    let started_at = Utc::now();
    let start_time = Instant::now();
    let trace_id = traces::new_trace_id();
    let span_id = traces::new_span_id();

    let (app_read, app_write) = app_stream.into_split();
    let (up_read, up_write) = tokio::io::split(upstream);

    let bytes_out = Arc::new(AtomicU64::new(0));
    let bytes_in = Arc::new(AtomicU64::new(0));

    let bo = bytes_out.clone();
    let copy_out = tokio::spawn(async move { outbound_counted_copy(app_read, up_write, bo).await });
    let bi = bytes_in.clone();
    let copy_in = tokio::spawn(async move { outbound_counted_copy(up_read, app_write, bi).await });

    let _ = tokio::try_join!(copy_out, copy_in);

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;
    let total = bytes_in.load(Ordering::Relaxed) + bytes_out.load(Ordering::Relaxed);

    let mut span = traces::build_connection_span(
        &trace_id,
        &span_id,
        &params.service_name,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        total,
    );
    if internal && let Some(serde_json::Value::Object(ref mut map)) = span.attributes {
        map.insert("mesh.internal".into(), serde_json::json!(true));
        map.insert("mesh.passthrough".into(), serde_json::json!(true));
    }
    let _ = params.span_tx.try_send(span);
    Ok(())
}

/// Bidirectional byte copy with counter for outbound proxy paths.
async fn outbound_counted_copy<R, W>(
    mut reader: R,
    mut writer: W,
    counter: Arc<AtomicU64>,
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
        counter.fetch_add(n as u64, Ordering::Relaxed);
        writer.write_all(&buf[..n]).await?;
    }
    writer.shutdown().await?;
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
