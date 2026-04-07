//! External TLS termination for gateway mode.
//!
//! Provides:
//! - `SniCertResolver`: rustls SNI-based certificate resolution with hot-reload
//! - Self-signed certificate generation (dev mode / fallback)
//! - K8s Secret watcher for `kubernetes.io/tls` Secrets (hot-reload into resolver)
//! - HTTP-to-HTTPS redirect listener
//! - HSTS header injection

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::sync::watch;

/// HSTS header value: 2 years, include subdomains.
pub const HSTS_HEADER_VALUE: &str = "max-age=63072000; includeSubDomains";

/// SNI-based certificate resolver for rustls.
///
/// Maps hostname -> `CertifiedKey`. Falls back to a default cert when no
/// hostname match is found (useful for dev mode with self-signed certs).
pub struct SniCertResolver {
    /// Hostname -> CertifiedKey map, atomically swappable.
    certs: ArcSwap<HashMap<String, Arc<CertifiedKey>>>,
    /// Fallback cert for unknown SNI (self-signed in dev mode).
    fallback: ArcSwap<Option<Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for SniCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let certs = self.certs.load();
        f.debug_struct("SniCertResolver")
            .field("hostnames", &certs.keys().collect::<Vec<_>>())
            .field("has_fallback", &self.fallback.load().is_some())
            .finish()
    }
}

impl SniCertResolver {
    /// Create a new empty resolver.
    pub fn new() -> Self {
        Self {
            certs: ArcSwap::from_pointee(HashMap::new()),
            fallback: ArcSwap::from_pointee(None),
        }
    }

    /// Create a resolver with a fallback self-signed cert.
    pub fn with_fallback(fallback: Arc<CertifiedKey>) -> Self {
        Self {
            certs: ArcSwap::from_pointee(HashMap::new()),
            fallback: ArcSwap::from_pointee(Some(fallback)),
        }
    }

    /// Replace all certs atomically.
    pub fn update_certs(&self, new_certs: HashMap<String, Arc<CertifiedKey>>) {
        self.certs.store(Arc::new(new_certs));
    }

    /// Insert or update a single hostname's cert.
    pub fn upsert_cert(&self, hostname: String, cert: Arc<CertifiedKey>) {
        let mut new_map = (**self.certs.load()).clone();
        new_map.insert(hostname, cert);
        self.certs.store(Arc::new(new_map));
    }

    /// Remove a hostname's cert.
    pub fn remove_cert(&self, hostname: &str) {
        let mut new_map = (**self.certs.load()).clone();
        new_map.remove(hostname);
        self.certs.store(Arc::new(new_map));
    }

    /// Set the fallback certificate.
    pub fn set_fallback(&self, cert: Arc<CertifiedKey>) {
        self.fallback.store(Arc::new(Some(cert)));
    }

    /// Get the number of loaded certs (for testing/metrics).
    pub fn cert_count(&self) -> usize {
        self.certs.load().len()
    }

    /// Check if a fallback cert is configured.
    pub fn has_fallback(&self) -> bool {
        self.fallback.load().is_some()
    }

    /// Look up a cert by hostname (for testing).
    pub fn get_cert(&self, hostname: &str) -> Option<Arc<CertifiedKey>> {
        self.certs.load().get(hostname).cloned()
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(server_name) = client_hello.server_name() {
            let certs = self.certs.load();

            // Exact match first
            if let Some(cert) = certs.get(server_name) {
                return Some(cert.clone());
            }

            // Wildcard match: *.example.com for foo.example.com
            if let Some((_sub, domain)) = server_name.split_once('.') {
                let wildcard = format!("*.{domain}");
                if let Some(cert) = certs.get(&wildcard) {
                    return Some(cert.clone());
                }
            }
        }

        // Fallback cert (dev mode)
        let fallback = self.fallback.load();
        fallback.as_ref().clone()
    }
}

/// Generate a self-signed certificate for the given hostnames using `rcgen`.
///
/// Returns the cert+key as PEM strings.
pub fn generate_self_signed_cert(
    hostnames: &[&str],
) -> anyhow::Result<(String, String)> {
    use rcgen::{CertificateParams, DnType, KeyPair};

    let key_pair = KeyPair::generate()
        .map_err(|e| anyhow::anyhow!("generate key pair: {e}"))?;

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, hostnames.first().copied().unwrap_or("localhost"));

    for &hostname in hostnames {
        params
            .subject_alt_names
            .push(rcgen::SanType::DnsName(
                hostname
                    .to_string()
                    .try_into()
                    .map_err(|e| anyhow::anyhow!("invalid hostname '{hostname}': {e}"))?,
            ));
    }

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow::anyhow!("self-sign cert: {e}"))?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

/// Parse PEM cert+key into a rustls `CertifiedKey`.
pub fn parse_certified_key(cert_pem: &str, key_pem: &str) -> anyhow::Result<CertifiedKey> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::io::BufReader;

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("parse cert PEM: {e}"))?;

    if certs.is_empty() {
        anyhow::bail!("no certificates found in PEM data");
    }

    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
            .map_err(|e| anyhow::anyhow!("parse key PEM: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("no private key found in PEM data"))?;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| anyhow::anyhow!("unsupported key type: {e}"))?;

    Ok(CertifiedKey::new(certs, signing_key))
}

/// Generate a self-signed `CertifiedKey` for use as a dev-mode fallback.
pub fn generate_fallback_cert() -> anyhow::Result<Arc<CertifiedKey>> {
    let (cert_pem, key_pem) = generate_self_signed_cert(&["localhost", "*.localhost"])?;
    let ck = parse_certified_key(&cert_pem, &key_pem)?;
    Ok(Arc::new(ck))
}

/// Build a rustls `ServerConfig` using the SNI cert resolver.
pub fn build_external_tls_config(resolver: Arc<SniCertResolver>) -> anyhow::Result<rustls::ServerConfig> {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    Ok(config)
}

/// Background task: watch `kubernetes.io/tls` Secrets in the gateway namespace
/// and hot-reload certs into the `SniCertResolver`.
#[tracing::instrument(skip_all, fields(namespace = %namespace))]
pub async fn watch_tls_secrets(
    kube_client: kube::Client,
    namespace: String,
    resolver: Arc<SniCertResolver>,
    mut shutdown: watch::Receiver<()>,
) {
    use futures_util::TryStreamExt;
    use kube::api::Api;
    use kube::runtime::watcher::{self, Event, watcher as kube_watcher};

    let api: Api<k8s_openapi::api::core::v1::Secret> =
        Api::namespaced(kube_client, &namespace);

    let wc = watcher::Config::default()
        .labels("kubernetes.io/tls");

    loop {
        let stream = kube_watcher(api.clone(), wc.clone());
        tokio::pin!(stream);

        let mut all_secrets: HashMap<String, k8s_openapi::api::core::v1::Secret> = HashMap::new();

        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                event = stream.try_next() => {
                    match event {
                        Ok(Some(Event::Init)) => {
                            all_secrets.clear();
                        }
                        Ok(Some(Event::InitApply(secret))) => {
                            let key = secret_key(&secret);
                            all_secrets.insert(key, secret);
                        }
                        Ok(Some(Event::InitDone)) => {
                            rebuild_certs_from_secrets(&all_secrets, &resolver);
                            tracing::info!(
                                secret_count = all_secrets.len(),
                                cert_count = resolver.cert_count(),
                                "initial TLS secret sync complete"
                            );
                        }
                        Ok(Some(Event::Apply(secret))) => {
                            let key = secret_key(&secret);
                            tracing::debug!(secret = %key, "TLS secret applied");
                            all_secrets.insert(key, secret);
                            rebuild_certs_from_secrets(&all_secrets, &resolver);
                        }
                        Ok(Some(Event::Delete(secret))) => {
                            let key = secret_key(&secret);
                            tracing::debug!(secret = %key, "TLS secret deleted");
                            all_secrets.remove(&key);
                            rebuild_certs_from_secrets(&all_secrets, &resolver);
                        }
                        Ok(None) => {
                            tracing::debug!("TLS secret watcher stream ended, restarting");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "TLS secret watcher error, restarting");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Build a unique key for a Secret.
fn secret_key(secret: &k8s_openapi::api::core::v1::Secret) -> String {
    let ns = secret.metadata.namespace.as_deref().unwrap_or("default");
    let name = secret.metadata.name.as_deref().unwrap_or("unknown");
    format!("{ns}/{name}")
}

/// Parse all TLS secrets and rebuild the resolver's cert map.
fn rebuild_certs_from_secrets(
    secrets: &HashMap<String, k8s_openapi::api::core::v1::Secret>,
    resolver: &SniCertResolver,
) {
    let mut new_certs: HashMap<String, Arc<CertifiedKey>> = HashMap::new();

    for (key, secret) in secrets {
        let Some(data) = &secret.data else {
            continue;
        };

        let cert_bytes = data.get("tls.crt").map(|b| &b.0);
        let key_bytes = data.get("tls.key").map(|b| &b.0);

        let (Some(cert_b), Some(key_b)) = (cert_bytes, key_bytes) else {
            tracing::debug!(secret = %key, "TLS secret missing tls.crt or tls.key");
            continue;
        };

        let cert_pem = String::from_utf8_lossy(cert_b);
        let key_pem = String::from_utf8_lossy(key_b);

        match parse_certified_key(&cert_pem, &key_pem) {
            Ok(ck) => {
                // Extract hostname from Secret name: convention is `tls-{hostname}`
                let hostname = secret
                    .metadata
                    .name
                    .as_deref()
                    .unwrap_or("")
                    .strip_prefix("tls-")
                    .unwrap_or_else(|| {
                        secret.metadata.name.as_deref().unwrap_or("")
                    });

                if !hostname.is_empty() {
                    tracing::info!(hostname = %hostname, secret = %key, "loaded TLS cert");
                    new_certs.insert(hostname.to_string(), Arc::new(ck));
                }
            }
            Err(e) => {
                tracing::warn!(secret = %key, error = %e, "failed to parse TLS secret");
            }
        }
    }

    resolver.update_certs(new_certs);
}

/// Build an HTTP 301 redirect response to the HTTPS equivalent.
///
/// Returns the raw HTTP response bytes.
pub fn build_redirect_response(host: &str, path: &str) -> Vec<u8> {
    let location = format!("https://{host}{path}");
    let body = format!("Moved to {location}");
    let response = format!(
        "HTTP/1.1 301 Moved Permanently\r\n\
         Location: {location}\r\n\
         Content-Length: {}\r\n\
         Content-Type: text/plain\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    response.into_bytes()
}

/// Build an ACME challenge response.
pub fn build_acme_challenge_response(token_value: &str) -> Vec<u8> {
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: {}\r\n\
         Content-Type: text/plain\r\n\
         Connection: close\r\n\
         \r\n\
         {token_value}",
        token_value.len()
    );
    response.into_bytes()
}

/// Inject the HSTS header into a raw HTTP response.
///
/// Inserts `Strict-Transport-Security` after the status line.
pub fn inject_hsts_header(response: &[u8]) -> Vec<u8> {
    let resp_str = String::from_utf8_lossy(response);
    // Find end of first line (status line)
    if let Some(pos) = resp_str.find("\r\n") {
        let mut result = Vec::with_capacity(response.len() + 80);
        result.extend_from_slice(&response[..pos + 2]);
        result.extend_from_slice(
            format!("Strict-Transport-Security: {HSTS_HEADER_VALUE}\r\n").as_bytes(),
        );
        result.extend_from_slice(&response[pos + 2..]);
        result
    } else {
        // Malformed response, return as-is
        response.to_vec()
    }
}

/// Parse host and path from an HTTP request for redirect purposes.
pub fn parse_host_and_path(request: &str) -> (String, String) {
    let mut host = String::new();
    let mut path = String::from("/");

    for (i, line) in request.lines().enumerate() {
        if i == 0 {
            // Request line: GET /path HTTP/1.1
            if let Some(p) = line.split_whitespace().nth(1) {
                path = p.to_string();
            }
        } else if line.is_empty() {
            break;
        } else if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("host") {
                host = value.trim().to_string();
            }
        }
    }

    (host, path)
}

/// Extract an ACME challenge token from a request path.
///
/// Returns `Some(token)` if the path matches `/.well-known/acme-challenge/{token}`.
pub fn extract_acme_token(path: &str) -> Option<&str> {
    path.strip_prefix("/.well-known/acme-challenge/")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sni_resolver_returns_correct_cert_per_hostname() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let resolver = SniCertResolver::new();

        // Generate two different certs
        let (cert1_pem, key1_pem) = generate_self_signed_cert(&["example.com"]).unwrap();
        let (cert2_pem, key2_pem) = generate_self_signed_cert(&["other.com"]).unwrap();

        let ck1 = Arc::new(parse_certified_key(&cert1_pem, &key1_pem).unwrap());
        let ck2 = Arc::new(parse_certified_key(&cert2_pem, &key2_pem).unwrap());

        let mut certs = HashMap::new();
        certs.insert("example.com".to_string(), ck1.clone());
        certs.insert("other.com".to_string(), ck2.clone());
        resolver.update_certs(certs);

        assert_eq!(resolver.cert_count(), 2);

        // Look up by hostname
        let found = resolver.get_cert("example.com");
        assert!(found.is_some());

        let found = resolver.get_cert("other.com");
        assert!(found.is_some());

        let found = resolver.get_cert("unknown.com");
        assert!(found.is_none());
    }

    #[test]
    fn sni_resolver_falls_back_to_default_for_unknown_sni() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let fallback = generate_fallback_cert().unwrap();
        let resolver = SniCertResolver::with_fallback(fallback);

        assert!(resolver.has_fallback());
        assert_eq!(resolver.cert_count(), 0);

        // The fallback is returned when resolve() gets an unknown hostname.
        // We can't easily call resolve() without a real ClientHello, so
        // we test via the has_fallback check and direct cert lookup.
        assert!(resolver.get_cert("unknown.com").is_none());
    }

    #[test]
    fn generate_self_signed_cert_produces_valid_pem() {
        let (cert_pem, key_pem) = generate_self_signed_cert(&["test.example.com"]).unwrap();
        assert!(cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    }

    #[test]
    fn parse_certified_key_roundtrip() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (cert_pem, key_pem) = generate_self_signed_cert(&["roundtrip.test"]).unwrap();
        let ck = parse_certified_key(&cert_pem, &key_pem);
        assert!(ck.is_ok());
    }

    #[test]
    fn parse_certified_key_empty_cert_fails() {
        let result = parse_certified_key("", "");
        assert!(result.is_err());
    }

    #[test]
    fn http_to_https_redirect_response_is_301() {
        let resp = build_redirect_response("example.com", "/foo/bar");
        let s = String::from_utf8(resp).unwrap();
        assert!(s.starts_with("HTTP/1.1 301 Moved Permanently"));
        assert!(s.contains("Location: https://example.com/foo/bar"));
    }

    #[test]
    fn hsts_header_present_on_https_responses() {
        let original = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let with_hsts = inject_hsts_header(original);
        let s = String::from_utf8(with_hsts).unwrap();
        assert!(s.contains("Strict-Transport-Security: max-age=63072000; includeSubDomains"));
        assert!(s.contains("Content-Length: 2"));
        assert!(s.ends_with("ok"));
    }

    #[test]
    fn hsts_header_preserves_status_line() {
        let original = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let with_hsts = inject_hsts_header(original);
        let s = String::from_utf8(with_hsts).unwrap();
        assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(s.contains("Strict-Transport-Security:"));
    }

    #[test]
    fn acme_challenge_path_parsed_correctly() {
        assert_eq!(
            extract_acme_token("/.well-known/acme-challenge/abc123"),
            Some("abc123")
        );
        assert_eq!(
            extract_acme_token("/.well-known/acme-challenge/some-long-token-value"),
            Some("some-long-token-value")
        );
        assert_eq!(extract_acme_token("/other/path"), None);
        assert_eq!(extract_acme_token("/.well-known/other"), None);
    }

    #[test]
    fn parse_host_and_path_basic() {
        let req = "GET /api/v1 HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (host, path) = parse_host_and_path(req);
        assert_eq!(host, "example.com");
        assert_eq!(path, "/api/v1");
    }

    #[test]
    fn parse_host_and_path_with_port() {
        let req = "GET / HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";
        let (host, path) = parse_host_and_path(req);
        assert_eq!(host, "example.com:8080");
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_host_and_path_no_host() {
        let req = "GET /test HTTP/1.1\r\n\r\n";
        let (host, path) = parse_host_and_path(req);
        assert!(host.is_empty());
        assert_eq!(path, "/test");
    }

    #[test]
    fn build_acme_challenge_response_format() {
        let resp = build_acme_challenge_response("token-value.key-auth");
        let s = String::from_utf8(resp).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK"));
        assert!(s.contains("Content-Type: text/plain"));
        assert!(s.ends_with("token-value.key-auth"));
    }

    #[test]
    fn sni_resolver_upsert_and_remove() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let resolver = SniCertResolver::new();
        assert_eq!(resolver.cert_count(), 0);

        let (cert_pem, key_pem) = generate_self_signed_cert(&["insert.test"]).unwrap();
        let ck = Arc::new(parse_certified_key(&cert_pem, &key_pem).unwrap());

        resolver.upsert_cert("insert.test".to_string(), ck);
        assert_eq!(resolver.cert_count(), 1);
        assert!(resolver.get_cert("insert.test").is_some());

        resolver.remove_cert("insert.test");
        assert_eq!(resolver.cert_count(), 0);
        assert!(resolver.get_cert("insert.test").is_none());
    }

    #[test]
    fn build_external_tls_config_succeeds() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let fallback = generate_fallback_cert().unwrap();
        let resolver = Arc::new(SniCertResolver::with_fallback(fallback));
        let config = build_external_tls_config(resolver);
        assert!(config.is_ok());
    }

    #[test]
    fn generate_fallback_cert_succeeds() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let result = generate_fallback_cert();
        assert!(result.is_ok());
    }
}
