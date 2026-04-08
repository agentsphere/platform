//! TLS certificate bootstrap, renewal, and config builders.

use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::watch;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use super::config::ProxyConfig;

/// Certificate bundle fetched from the platform CA.
#[derive(Debug, Clone)]
pub struct ProxyCerts {
    pub cert_pem: String,
    pub key_pem: String,
    pub ca_pem: String,
    pub not_after: DateTime<Utc>,
}

impl ProxyCerts {
    /// Create an empty placeholder (no valid certs yet).
    /// mTLS listeners will reject connections until real certs are swapped in.
    pub fn empty() -> Self {
        Self {
            cert_pem: String::new(),
            key_pem: String::new(),
            ca_pem: String::new(),
            not_after: Utc::now(),
        }
    }

    /// Whether this cert bundle has been populated with real certs.
    pub fn is_valid(&self) -> bool {
        !self.cert_pem.is_empty()
    }
}

/// Shared, hot-swappable certs handle.
pub type SharedCerts = Arc<ArcSwap<ProxyCerts>>;

/// Response from the platform cert issuance API.
#[derive(Debug, serde::Deserialize)]
struct CertIssueResponse {
    cert_pem: String,
    key_pem: String,
    ca_pem: String,
    not_after: DateTime<Utc>,
    #[allow(dead_code)]
    spiffe_id: Option<String>,
}

/// Fetch initial cert from platform CA.
///
/// `POST {PLATFORM_API_URL}/api/mesh/certs/issue`
/// Auth: `Bearer {PLATFORM_API_TOKEN}`
#[tracing::instrument(skip(config), fields(namespace = %config.namespace, service = %config.service_name))]
pub async fn bootstrap_cert(config: &ProxyConfig) -> anyhow::Result<ProxyCerts> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let url = format!("{}/api/mesh/certs/issue", config.api_url);
    let body = serde_json::json!({
        "namespace": config.namespace,
        "service": config.service_name,
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_token))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("cert bootstrap request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("cert bootstrap failed: HTTP {status}: {text}");
    }

    let cert_resp: CertIssueResponse = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("cert bootstrap: invalid response JSON: {e}"))?;

    tracing::info!(
        not_after = %cert_resp.not_after,
        "certificate bootstrapped"
    );

    Ok(ProxyCerts {
        cert_pem: cert_resp.cert_pem,
        key_pem: cert_resp.key_pem,
        ca_pem: cert_resp.ca_pem,
        not_after: cert_resp.not_after,
    })
}

/// Background task: renew cert at 50% lifetime.
///
/// On failure: retry with exponential backoff (1s, 2s, 4s, ..., 30s max).
/// On persistent failure: log error, continue with existing cert until expiry.
#[tracing::instrument(skip_all)]
pub async fn cert_renewal_loop(
    config: ProxyConfig,
    certs: SharedCerts,
    mut shutdown: watch::Receiver<()>,
) {
    loop {
        let current = certs.load();
        let now = Utc::now();
        let lifetime = current.not_after - now;
        let renewal_delay = lifetime / 2;
        let sleep_secs = u64::try_from(renewal_delay.num_seconds().max(1)).unwrap_or(1);

        tracing::debug!(
            sleep_secs,
            not_after = %current.not_after,
            "scheduling cert renewal"
        );

        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {
                // Attempt renewal with exponential backoff
                let mut backoff = Duration::from_secs(1);
                let max_backoff = Duration::from_secs(30);

                loop {
                    match bootstrap_cert(&config).await {
                        Ok(new_certs) => {
                            tracing::info!(
                                not_after = %new_certs.not_after,
                                "certificate renewed"
                            );
                            certs.store(Arc::new(new_certs));
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                backoff_secs = backoff.as_secs(),
                                "cert renewal failed, retrying"
                            );
                            tokio::select! {
                                () = tokio::time::sleep(backoff) => {}
                                _ = shutdown.changed() => return,
                            }
                            backoff = (backoff * 2).min(max_backoff);
                        }
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("cert renewal loop exiting");
}

/// Build a rustls `TlsAcceptor` from the given certs.
///
/// Configures mutual TLS: server presents its cert, requires client certs
/// verified against the CA trust bundle.
pub fn build_tls_acceptor(certs: &ProxyCerts) -> anyhow::Result<TlsAcceptor> {
    let server_certs = load_certs_from_pem(&certs.cert_pem)?;
    let server_key = load_key_from_pem(&certs.key_pem)?;

    // Build CA verifier for client certs
    let ca_certs = load_certs_from_pem(&certs.ca_pem)?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in &ca_certs {
        root_store
            .add(cert.clone())
            .map_err(|e| anyhow::anyhow!("failed to add CA cert: {e}"))?;
    }

    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build client verifier: {e}"))?;

    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(server_certs, server_key)
        .map_err(|e| anyhow::anyhow!("failed to build TLS server config: {e}"))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Build a permissive `TlsAcceptor` that accepts but does not require client certs.
///
/// Used in transparent-proxy permissive mode: if the peer presents a cert it is
/// validated against the CA trust bundle; if it sends none the handshake still
/// succeeds.
pub fn build_permissive_tls_acceptor(certs: &ProxyCerts) -> anyhow::Result<TlsAcceptor> {
    let server_certs = load_certs_from_pem(&certs.cert_pem)?;
    let server_key = load_key_from_pem(&certs.key_pem)?;

    let ca_certs = load_certs_from_pem(&certs.ca_pem)?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in &ca_certs {
        root_store
            .add(cert.clone())
            .map_err(|e| anyhow::anyhow!("failed to add CA cert: {e}"))?;
    }

    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .allow_unauthenticated()
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build permissive client verifier: {e}"))?;

    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(server_certs, server_key)
        .map_err(|e| anyhow::anyhow!("failed to build permissive TLS server config: {e}"))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Build a rustls `TlsConnector` for outbound mTLS origination.
///
/// Presents the proxy's client cert to the upstream.
pub fn build_tls_connector(certs: &ProxyCerts) -> anyhow::Result<TlsConnector> {
    let client_certs = load_certs_from_pem(&certs.cert_pem)?;
    let client_key = load_key_from_pem(&certs.key_pem)?;

    // Trust the platform CA
    let ca_certs = load_certs_from_pem(&certs.ca_pem)?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in &ca_certs {
        root_store
            .add(cert.clone())
            .map_err(|e| anyhow::anyhow!("failed to add CA cert: {e}"))?;
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(client_certs, client_key)
        .map_err(|e| anyhow::anyhow!("failed to build TLS client config: {e}"))?;

    Ok(TlsConnector::from(Arc::new(config)))
}

/// Parse PEM-encoded certificates.
fn load_certs_from_pem(pem: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("failed to parse PEM certs: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in PEM data");
    }
    Ok(certs)
}

/// Parse a PEM-encoded private key (RSA, PKCS8, or EC).
fn load_key_from_pem(pem: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| anyhow::anyhow!("failed to parse PEM key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in PEM data"))
}

/// Extract the SPIFFE ID from a client certificate's SAN (URI type).
///
/// Looks for URIs matching `spiffe://platform/{namespace}/{service}`.
pub fn extract_spiffe_id(cert_der: &[u8]) -> Option<String> {
    // Parse the DER-encoded certificate to extract SAN URIs.
    // Use a simple approach: look for the SPIFFE URI prefix in the raw bytes.
    // A full X.509 parser would be better, but we avoid adding x509-parser
    // as a dep for the proxy binary since it's only used here.
    let cert_str = String::from_utf8_lossy(cert_der);
    // In DER encoding, the SPIFFE URI is embedded as a UTF-8 string
    // within the SubjectAltName extension. We search for it directly.
    let prefix = "spiffe://platform/";
    if let Some(start) = cert_str.find(prefix) {
        // Extract until the next non-printable character or end
        let remaining = &cert_str[start..];
        let end = remaining
            .find(|c: char| {
                !c.is_ascii_alphanumeric()
                    && c != '/'
                    && c != '-'
                    && c != '_'
                    && c != ':'
                    && c != '.'
            })
            .unwrap_or(remaining.len());
        Some(remaining[..end].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Self-signed test cert/key pair for TLS config testing.
    // Generated with: rcgen (or similar). We use a minimal inline cert here.
    // Note: In real tests, these would come from the mesh CA. For unit testing
    // the TLS config builder, we use a pre-generated self-signed pair.

    // We can't easily generate certs without rcgen (which is a dev dep of the
    // main binary, not the proxy). So we test the PEM parsing functions.

    #[test]
    fn load_certs_from_empty_pem() {
        let result = load_certs_from_pem("");
        assert!(result.is_err());
    }

    #[test]
    fn load_key_from_empty_pem() {
        let result = load_key_from_pem("");
        assert!(result.is_err());
    }

    #[test]
    fn load_certs_from_invalid_pem() {
        let result = load_certs_from_pem("not a pem");
        assert!(result.is_err());
    }

    #[test]
    fn extract_spiffe_id_from_string() {
        // Simulate DER bytes that contain the SPIFFE URI terminated by \0 (non-printable)
        let fake_der = b"\x01\x02spiffe://platform/default/postgres\x00more-bytes";
        let id = extract_spiffe_id(fake_der);
        assert_eq!(id, Some("spiffe://platform/default/postgres".to_string()));
    }

    #[test]
    fn extract_spiffe_id_missing() {
        let fake_der = b"no spiffe here";
        let id = extract_spiffe_id(fake_der);
        assert!(id.is_none());
    }

    #[test]
    fn proxy_certs_struct() {
        let certs = ProxyCerts {
            cert_pem: "cert".into(),
            key_pem: "key".into(),
            ca_pem: "ca".into(),
            not_after: Utc::now(),
        };
        assert_eq!(certs.cert_pem, "cert");
        assert_eq!(certs.key_pem, "key");
        assert_eq!(certs.ca_pem, "ca");
    }
}
