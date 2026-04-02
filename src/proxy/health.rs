//! Health HTTP server for proxy liveness and readiness probes.

use std::net::SocketAddr;

use tokio::net::TcpStream;
use tokio::sync::watch;

/// Run the health HTTP server on the given port.
///
/// - `GET /healthz` -> 200 if proxy is running
/// - `GET /readyz`  -> 200 if child app port is reachable, 503 otherwise
#[tracing::instrument(skip_all, fields(port))]
pub async fn run_health_server(
    port: u16,
    app_port: Option<u16>,
    mut shutdown: watch::Receiver<()>,
) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, port, "failed to bind health server");
            return;
        }
    };
    tracing::info!(port, "health server listening");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let app_port = app_port;
                        tokio::spawn(async move {
                            if let Err(e) = handle_health_request(stream, app_port).await {
                                tracing::debug!(error = %e, "health request error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "health accept error");
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("health server exiting");
}

/// Handle a single HTTP health request using minimal HTTP/1.1 parsing.
async fn handle_health_request(
    mut stream: tokio::net::TcpStream,
    app_port: Option<u16>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status, body) = match path {
        "/healthz" => ("200 OK", "ok"),
        "/readyz" => {
            if let Some(port) = app_port {
                if check_port_reachable(port).await {
                    ("200 OK", "ready")
                } else {
                    ("503 Service Unavailable", "not ready")
                }
            } else {
                // No app port configured — report ready (proxy-only mode)
                ("200 OK", "ready")
            }
        }
        _ => ("404 Not Found", "not found"),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

/// Check if a TCP port on localhost is reachable.
async fn check_port_reachable(port: u16) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        TcpStream::connect(format!("127.0.0.1:{port}")),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_port_unreachable() {
        // Port 1 should never be reachable
        assert!(!check_port_reachable(1).await);
    }
}
