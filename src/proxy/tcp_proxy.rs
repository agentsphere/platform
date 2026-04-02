//! TCP proxy mode: mTLS listener -> plain TCP to localhost.
//!
//! For non-HTTP protocols like Postgres and Redis. Creates one CONNECTION
//! span per TCP session tracking start time, end time, and bytes transferred.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::tls::{self, SharedCerts};
use super::traces::{self, SpanRecord};

/// Run a TCP proxy for a single port pair.
///
/// Listens on `0.0.0.0:{tls_port}` with mTLS, forwards plaintext to
/// `127.0.0.1:{upstream_port}`. Creates one CONNECTION span per session.
#[tracing::instrument(skip_all, fields(tls_port, upstream_port))]
pub async fn run_tcp_proxy(
    tls_port: u16,
    upstream_port: u16,
    service_name: String,
    certs: SharedCerts,
    span_tx: mpsc::Sender<SpanRecord>,
    mut shutdown: watch::Receiver<()>,
) {
    let addr = SocketAddr::from(([0, 0, 0, 0], tls_port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, tls_port, "failed to bind TCP proxy");
            return;
        }
    };
    tracing::info!(tls_port, upstream_port, "TCP proxy started");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        let certs = certs.clone();
                        let span_tx = span_tx.clone();
                        let service = service_name.clone();

                        tokio::spawn(async move {
                            if let Err(e) = handle_tcp_connection(
                                stream, peer, upstream_port, certs, span_tx, &service,
                            ).await {
                                tracing::debug!(
                                    error = %e,
                                    peer = %peer,
                                    "TCP proxy connection error"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "TCP proxy accept error");
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("TCP proxy exiting");
}

/// Handle a single TCP proxy connection.
async fn handle_tcp_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    upstream_port: u16,
    certs: SharedCerts,
    span_tx: mpsc::Sender<SpanRecord>,
    service: &str,
) -> anyhow::Result<()> {
    let current_certs = certs.load();
    let acceptor = tls::build_tls_acceptor(&current_certs)?;

    // TLS handshake
    let tls_stream = acceptor
        .accept(stream)
        .await
        .map_err(|e| anyhow::anyhow!("TCP proxy TLS handshake failed from {peer}: {e}"))?;

    // Connect to upstream
    let upstream = tokio::net::TcpStream::connect(format!("127.0.0.1:{upstream_port}"))
        .await
        .map_err(|e| {
            anyhow::anyhow!("TCP proxy: failed to connect to upstream port {upstream_port}: {e}")
        })?;

    let started_at = Utc::now();
    let start_time = std::time::Instant::now();
    let trace_id = traces::new_trace_id();
    let span_id = traces::new_span_id();

    // Split TLS stream and upstream for bidirectional copy
    let (tls_read, tls_write) = tokio::io::split(tls_stream);
    let (upstream_read, upstream_write) = tokio::io::split(upstream);

    let inbound_bytes = Arc::new(AtomicU64::new(0));
    let outbound_bytes = Arc::new(AtomicU64::new(0));

    let inbound_counter = inbound_bytes.clone();
    let outbound_counter = outbound_bytes.clone();

    // Copy in both directions
    let copy_in =
        tokio::spawn(async move { counted_copy(tls_read, upstream_write, inbound_counter).await });
    let copy_out =
        tokio::spawn(async move { counted_copy(upstream_read, tls_write, outbound_counter).await });

    // Wait for either direction to finish
    let _ = tokio::try_join!(copy_in, copy_out);

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;
    let total_bytes =
        inbound_bytes.load(Ordering::Relaxed) + outbound_bytes.load(Ordering::Relaxed);

    // Build CONNECTION span
    let span = traces::build_connection_span(
        &trace_id,
        &span_id,
        service,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        total_bytes,
    );
    let _ = span_tx.try_send(span);

    tracing::debug!(
        peer = %peer,
        bytes = total_bytes,
        duration_ms,
        "TCP proxy session ended"
    );

    Ok(())
}

/// Copy bytes from reader to writer, counting bytes transferred.
async fn counted_copy<R, W>(
    mut reader: R,
    mut writer: W,
    counter: Arc<AtomicU64>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncReadExt;

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
