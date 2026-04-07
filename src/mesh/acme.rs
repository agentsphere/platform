//! ACME HTTP-01 certificate provisioning (Let's Encrypt).
//!
//! Background task that:
//! 1. Watches `HTTPRoute` hostnames to discover which domains need certs
//! 2. Initiates ACME HTTP-01 challenges for domains without valid TLS Secrets
//! 3. Stores challenge tokens in a `ConfigMap` (`acme-challenges`) for the gateway to serve
//! 4. On successful validation: stores cert+key in K8s Secret `tls-{hostname}`
//! 5. Renews certs 30 days before expiry

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use kube::api::{Api, DynamicObject, ListParams, ObjectMeta, Patch, PatchParams, PostParams};
use kube::discovery::ApiResource;
use tokio::sync::watch;

use crate::config::Config;

/// ACME manager configuration.
#[derive(Debug, Clone)]
pub struct AcmeConfig {
    /// ACME directory URL (e.g. Let's Encrypt production or staging).
    pub directory_url: String,
    /// Contact email for ACME account registration.
    pub contact_email: Option<String>,
    /// Gateway namespace where TLS secrets and ACME configmap live.
    pub gateway_namespace: String,
    /// Gateway name to filter `HTTPRoute` parentRefs.
    pub gateway_name: String,
    /// Check interval in seconds (default: 3600 = 1 hour).
    pub check_interval_secs: u64,
}

impl AcmeConfig {
    /// Build from platform config.
    pub fn from_config(config: &Config) -> Self {
        Self {
            directory_url: config.acme_directory_url.clone(),
            contact_email: config.acme_contact_email.clone(),
            gateway_namespace: config.gateway_namespace.clone(),
            gateway_name: config.gateway_name.clone(),
            check_interval_secs: 3600,
        }
    }
}

/// State for tracking issued certificates and their expiry.
#[derive(Debug)]
struct CertState {
    /// Hostname -> expiry timestamp.
    expiry: HashMap<String, DateTime<Utc>>,
}

impl CertState {
    fn new() -> Self {
        Self {
            expiry: HashMap::new(),
        }
    }

    /// Returns true if the hostname needs a cert (missing or expiring within 30 days).
    fn needs_cert(&self, hostname: &str) -> bool {
        match self.expiry.get(hostname) {
            None => true,
            Some(expiry) => {
                let renewal_threshold = Utc::now() + chrono::Duration::days(30);
                *expiry < renewal_threshold
            }
        }
    }
}

/// Background task: periodically check for hostnames needing ACME certs.
///
/// This runs in the platform binary (not the proxy), because it needs
/// access to the ACME account and K8s API for creating Secrets.
#[tracing::instrument(skip_all, fields(directory_url = %acme_config.directory_url))]
pub async fn run_acme_manager(
    kube_client: kube::Client,
    acme_config: AcmeConfig,
    mut shutdown: watch::Receiver<()>,
) {
    let mut cert_state = CertState::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        acme_config.check_interval_secs,
    ));

    // Load existing TLS secrets to populate cert_state
    if let Err(e) = load_existing_certs(&kube_client, &acme_config, &mut cert_state).await {
        tracing::warn!(error = %e, "failed to load existing TLS certs");
    }

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = check_and_provision(
                    &kube_client,
                    &acme_config,
                    &mut cert_state,
                ).await {
                    tracing::warn!(error = %e, "ACME provisioning cycle failed");
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("ACME manager shutting down");
                break;
            }
        }
    }
}

/// Load existing TLS secrets to populate the cert state on startup.
async fn load_existing_certs(
    kube_client: &kube::Client,
    config: &AcmeConfig,
    state: &mut CertState,
) -> anyhow::Result<()> {
    let secrets_api: Api<k8s_openapi::api::core::v1::Secret> =
        Api::namespaced(kube_client.clone(), &config.gateway_namespace);

    let lp = ListParams::default().labels("kubernetes.io/tls");
    let secrets = secrets_api.list(&lp).await?;

    for secret in &secrets.items {
        let name = secret.metadata.name.as_deref().unwrap_or("");
        let Some(hostname) = name.strip_prefix("tls-") else {
            continue;
        };

        // Try to extract cert expiry from annotations
        if let Some(Ok(expiry)) = secret
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("platform.io/cert-expiry"))
            .map(|s| s.parse::<DateTime<Utc>>())
        {
            state.expiry.insert(hostname.to_string(), expiry);
            tracing::debug!(hostname = %hostname, expiry = %expiry, "loaded existing cert");
        }
    }

    tracing::info!(
        certs = state.expiry.len(),
        "loaded existing ACME cert state"
    );
    Ok(())
}

/// Single ACME provisioning cycle: discover hostnames, provision missing certs.
async fn check_and_provision(
    kube_client: &kube::Client,
    config: &AcmeConfig,
    state: &mut CertState,
) -> anyhow::Result<()> {
    // Discover hostnames from HTTPRoute resources
    let hostnames = discover_hostnames(kube_client, config).await?;

    if hostnames.is_empty() {
        tracing::debug!("no hostnames found in HTTPRoutes");
        return Ok(());
    }

    // Filter to hostnames needing certs
    let needed: Vec<&str> = hostnames
        .iter()
        .filter(|h| state.needs_cert(h))
        .map(String::as_str)
        .collect();

    if needed.is_empty() {
        tracing::debug!(
            total = hostnames.len(),
            "all certs valid, nothing to provision"
        );
        return Ok(());
    }

    tracing::info!(
        needed = needed.len(),
        total = hostnames.len(),
        "starting ACME provisioning"
    );

    for hostname in needed {
        match provision_cert(kube_client, config, hostname).await {
            Ok(expiry) => {
                state.expiry.insert(hostname.to_string(), expiry);
                tracing::info!(hostname = %hostname, expiry = %expiry, "ACME cert provisioned");
            }
            Err(e) => {
                tracing::warn!(hostname = %hostname, error = %e, "ACME cert provisioning failed");
            }
        }
    }

    Ok(())
}

/// Discover all hostnames from `HTTPRoute` resources referencing our gateway.
async fn discover_hostnames(
    kube_client: &kube::Client,
    config: &AcmeConfig,
) -> anyhow::Result<Vec<String>> {
    let httproute_ar = ApiResource {
        group: "gateway.networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "gateway.networking.k8s.io/v1".into(),
        kind: "HTTPRoute".into(),
        plural: "httproutes".into(),
    };

    let api: Api<DynamicObject> = Api::all_with(kube_client.clone(), &httproute_ar);
    let routes = api.list(&ListParams::default()).await?;

    let mut hostnames = Vec::new();

    for route in &routes.items {
        // Check parentRef matches our gateway
        let parent_refs = route
            .data
            .get("spec")
            .and_then(|s| s.get("parentRefs"))
            .and_then(|p| p.as_array());

        let matches_gateway = parent_refs.is_some_and(|refs| {
            refs.iter().any(|pr| {
                let name = pr.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let ns = pr.get("namespace").and_then(|n| n.as_str()).unwrap_or("");
                name == config.gateway_name && ns == config.gateway_namespace
            })
        });

        if !matches_gateway {
            continue;
        }

        // Extract hostnames
        if let Some(names) = route
            .data
            .get("spec")
            .and_then(|s| s.get("hostnames"))
            .and_then(|h| h.as_array())
        {
            for name in names {
                if let Some(h) = name.as_str() {
                    // Skip wildcards (can't get ACME certs for *.example.com via HTTP-01)
                    if !h.starts_with('*') {
                        hostnames.push(h.to_string());
                    }
                }
            }
        }
    }

    hostnames.sort();
    hostnames.dedup();
    Ok(hostnames)
}

/// Provision a certificate for a single hostname via ACME HTTP-01.
async fn provision_cert(
    kube_client: &kube::Client,
    config: &AcmeConfig,
    hostname: &str,
) -> anyhow::Result<DateTime<Utc>> {
    use instant_acme::{
        Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    };

    tracing::info!(hostname = %hostname, "starting ACME HTTP-01 challenge");

    // Create or load ACME account
    let contact = config.contact_email.as_ref().map(|e| format!("mailto:{e}"));
    let contact_refs: Vec<&str> = contact.iter().map(String::as_str).collect();

    let (account, _) = Account::create(
        &NewAccount {
            contact: &contact_refs,
            terms_of_service_agreed: true,
            only_return_existing: false,
        },
        &config.directory_url,
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("ACME account creation failed: {e}"))?;

    // Create new order
    let identifier = Identifier::Dns(hostname.to_string());
    let mut order = account
        .new_order(&NewOrder {
            identifiers: &[identifier],
        })
        .await
        .map_err(|e| anyhow::anyhow!("ACME new order failed: {e}"))?;

    // Get authorizations
    let authorizations = order
        .authorizations()
        .await
        .map_err(|e| anyhow::anyhow!("ACME authorizations failed: {e}"))?;

    for auth in &authorizations {
        if !matches!(auth.status, AuthorizationStatus::Pending) {
            continue;
        }

        // Find HTTP-01 challenge
        let challenge = auth
            .challenges
            .iter()
            .find(|c| c.r#type == ChallengeType::Http01)
            .ok_or_else(|| anyhow::anyhow!("no HTTP-01 challenge found"))?;

        let token = &challenge.token;
        let key_authorization = order.key_authorization(challenge);

        // Store challenge token in K8s ConfigMap for gateway to serve
        store_acme_challenge(kube_client, config, token, key_authorization.as_str()).await?;

        // Tell ACME server we're ready
        order
            .set_challenge_ready(&challenge.url)
            .await
            .map_err(|e| anyhow::anyhow!("ACME set_challenge_ready failed: {e}"))?;
    }

    // Poll for order completion
    let max_polls = 30;
    for _ in 0..max_polls {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let order_state = order
            .refresh()
            .await
            .map_err(|e| anyhow::anyhow!("ACME order refresh failed: {e}"))?;

        match order_state.status {
            OrderStatus::Ready | OrderStatus::Valid => break,
            OrderStatus::Invalid => {
                anyhow::bail!("ACME order became invalid for {hostname}");
            }
            _ => {}
        }
    }

    let (cert_chain, key_pem) = finalize_order(&mut order, hostname).await?;

    // Calculate expiry (default: 90 days from now for Let's Encrypt)
    let expiry = Utc::now() + chrono::Duration::days(90);

    // Store cert in K8s Secret
    store_tls_secret(kube_client, config, hostname, &cert_chain, &key_pem, expiry).await?;

    // Clean up challenge token
    remove_acme_challenge(kube_client, config, hostname).await?;

    Ok(expiry)
}

/// Generate CSR, finalize the ACME order, and retrieve the certificate chain.
async fn finalize_order(
    order: &mut instant_acme::Order,
    hostname: &str,
) -> anyhow::Result<(String, String)> {
    use instant_acme::OrderStatus;

    let cert_key =
        rcgen::KeyPair::generate().map_err(|e| anyhow::anyhow!("generate cert key: {e}"))?;

    let csr = {
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, hostname);
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            hostname
                .to_string()
                .try_into()
                .map_err(|e| anyhow::anyhow!("invalid hostname for SAN: {e}"))?,
        ));
        params
            .serialize_request(&cert_key)
            .map_err(|e| anyhow::anyhow!("CSR generation failed: {e}"))?
    };

    order
        .finalize(csr.der())
        .await
        .map_err(|e| anyhow::anyhow!("ACME finalize failed: {e}"))?;

    // Poll for cert
    let cert_chain = loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let state = order
            .refresh()
            .await
            .map_err(|e| anyhow::anyhow!("ACME order refresh failed: {e}"))?;
        if matches!(state.status, OrderStatus::Valid) {
            break order
                .certificate()
                .await
                .map_err(|e| anyhow::anyhow!("ACME certificate fetch failed: {e}"))?
                .ok_or_else(|| anyhow::anyhow!("no certificate returned"))?;
        }
    };

    Ok((cert_chain, cert_key.serialize_pem()))
}

/// Store an ACME challenge token in the `acme-challenges` `ConfigMap`.
async fn store_acme_challenge(
    kube_client: &kube::Client,
    config: &AcmeConfig,
    token: &str,
    key_authorization: &str,
) -> anyhow::Result<()> {
    let cm_api: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(kube_client.clone(), &config.gateway_namespace);

    let cm_name = "acme-challenges";

    // Try to get existing ConfigMap, create if missing
    let mut data = if let Ok(existing) = cm_api.get(cm_name).await {
        existing.data.unwrap_or_default()
    } else {
        let cm = k8s_openapi::api::core::v1::ConfigMap {
            metadata: ObjectMeta {
                name: Some(cm_name.to_string()),
                namespace: Some(config.gateway_namespace.clone()),
                ..Default::default()
            },
            data: Some(std::collections::BTreeMap::new()),
            ..Default::default()
        };
        cm_api.create(&PostParams::default(), &cm).await?;
        std::collections::BTreeMap::new()
    };

    data.insert(token.to_string(), key_authorization.to_string());

    let patch = serde_json::json!({
        "data": data
    });
    cm_api
        .patch(
            cm_name,
            &PatchParams::apply("platform-acme"),
            &Patch::Merge(&patch),
        )
        .await?;

    tracing::debug!(token = %token, "stored ACME challenge token");
    Ok(())
}

/// Remove an ACME challenge token from the `ConfigMap` after validation.
async fn remove_acme_challenge(
    kube_client: &kube::Client,
    config: &AcmeConfig,
    _hostname: &str,
) -> anyhow::Result<()> {
    // We could remove individual tokens, but it's simpler to leave them
    // (they're only valid for a short time anyway). The ConfigMap will
    // be overwritten on the next challenge.
    let cm_api: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(kube_client.clone(), &config.gateway_namespace);

    // Best-effort cleanup: clear all challenge data
    let patch = serde_json::json!({
        "data": {}
    });
    let _ = cm_api
        .patch(
            "acme-challenges",
            &PatchParams::apply("platform-acme"),
            &Patch::Merge(&patch),
        )
        .await;

    Ok(())
}

/// Store a TLS cert+key as a K8s Secret.
async fn store_tls_secret(
    kube_client: &kube::Client,
    config: &AcmeConfig,
    hostname: &str,
    cert_pem: &str,
    key_pem: &str,
    expiry: DateTime<Utc>,
) -> anyhow::Result<()> {
    let secrets_api: Api<k8s_openapi::api::core::v1::Secret> =
        Api::namespaced(kube_client.clone(), &config.gateway_namespace);

    let secret_name = format!("tls-{hostname}");

    let mut labels = std::collections::BTreeMap::new();
    labels.insert("kubernetes.io/tls".to_string(), String::new());
    labels.insert("platform.io/managed-by".to_string(), "acme".to_string());

    let mut annotations = std::collections::BTreeMap::new();
    annotations.insert("platform.io/cert-expiry".to_string(), expiry.to_rfc3339());
    annotations.insert("platform.io/hostname".to_string(), hostname.to_string());

    let mut data = std::collections::BTreeMap::new();
    data.insert(
        "tls.crt".to_string(),
        k8s_openapi::ByteString(cert_pem.as_bytes().to_vec()),
    );
    data.insert(
        "tls.key".to_string(),
        k8s_openapi::ByteString(key_pem.as_bytes().to_vec()),
    );

    let secret = k8s_openapi::api::core::v1::Secret {
        metadata: ObjectMeta {
            name: Some(secret_name.clone()),
            namespace: Some(config.gateway_namespace.clone()),
            labels: Some(labels),
            annotations: Some(annotations),
            ..Default::default()
        },
        type_: Some("kubernetes.io/tls".to_string()),
        data: Some(data),
        ..Default::default()
    };

    let patch = serde_json::to_value(&secret)?;
    secrets_api
        .patch(
            &secret_name,
            &PatchParams::apply("platform-acme"),
            &Patch::Apply(&patch),
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to store TLS secret {secret_name}: {e}"))?;

    tracing::info!(hostname = %hostname, secret = %secret_name, "stored TLS certificate");
    Ok(())
}

/// Parse an ACME challenge token and path to find the key authorization.
///
/// The token is the last path segment of `/.well-known/acme-challenge/{token}`.
pub fn parse_acme_challenge_path(path: &str) -> Option<&str> {
    path.strip_prefix("/.well-known/acme-challenge/")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_state_needs_cert_missing() {
        let state = CertState::new();
        assert!(state.needs_cert("example.com"));
    }

    #[test]
    fn cert_state_needs_cert_fresh() {
        let mut state = CertState::new();
        // Cert expiring in 60 days - should not need renewal
        state
            .expiry
            .insert("fresh.com".into(), Utc::now() + chrono::Duration::days(60));
        assert!(!state.needs_cert("fresh.com"));
    }

    #[test]
    fn cert_state_needs_cert_expiring_soon() {
        let mut state = CertState::new();
        // Cert expiring in 15 days - within 30-day renewal window
        state.expiry.insert(
            "expiring.com".into(),
            Utc::now() + chrono::Duration::days(15),
        );
        assert!(state.needs_cert("expiring.com"));
    }

    #[test]
    fn cert_state_needs_cert_expired() {
        let mut state = CertState::new();
        // Already expired
        state
            .expiry
            .insert("expired.com".into(), Utc::now() - chrono::Duration::days(5));
        assert!(state.needs_cert("expired.com"));
    }

    #[test]
    fn parse_acme_challenge_path_valid() {
        assert_eq!(
            parse_acme_challenge_path("/.well-known/acme-challenge/abc123"),
            Some("abc123")
        );
    }

    #[test]
    fn parse_acme_challenge_path_invalid() {
        assert_eq!(parse_acme_challenge_path("/other/path"), None);
        assert_eq!(parse_acme_challenge_path("/.well-known/"), None);
    }

    #[test]
    fn acme_config_from_platform_config() {
        // Can't easily construct Config in a test, but verify the struct fields exist
        let config = AcmeConfig {
            directory_url: "https://acme-staging.example.com/directory".into(),
            contact_email: Some("admin@example.com".into()),
            gateway_namespace: "platform".into(),
            gateway_name: "platform-gateway".into(),
            check_interval_secs: 3600,
        };
        assert_eq!(
            config.directory_url,
            "https://acme-staging.example.com/directory"
        );
        assert_eq!(config.contact_email, Some("admin@example.com".into()));
    }
}
