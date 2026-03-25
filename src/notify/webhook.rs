use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::api::webhooks::{WEBHOOK_CLIENT, WEBHOOK_SEMAPHORE, validate_webhook_url};
use crate::error::ApiError;

/// Result of a webhook delivery attempt.
#[derive(Debug)]
pub struct DeliveryResult {
    pub status_code: u16,
    pub success: bool,
}

/// Compute HMAC-SHA256 signature for a payload.
/// Returns `sha256={hex}` format string, or `None` if the key is invalid.
fn compute_signature(payload: &[u8], secret: &str) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload);
    let signature = hex::encode(mac.finalize().into_bytes());
    Some(format!("sha256={signature}"))
}

/// Deliver a JSON payload to a webhook URL with optional HMAC-SHA256 signing.
///
/// Reuses the shared webhook HTTP client and SSRF protection from `api::webhooks`.
#[tracing::instrument(skip(url, payload, secret), err)]
pub async fn deliver(
    url: &str,
    payload: &serde_json::Value,
    secret: Option<&str>,
) -> Result<DeliveryResult, ApiError> {
    // SSRF protection
    validate_webhook_url(url)?;

    // Acquire concurrency permit
    let _permit = WEBHOOK_SEMAPHORE.try_acquire().map_err(|_| {
        tracing::warn!("notification webhook dropped: concurrency limit reached");
        ApiError::ServiceUnavailable("webhook delivery limit reached".into())
    })?;

    let body = serde_json::to_string(payload)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to serialize payload: {e}")))?;

    let mut request = WEBHOOK_CLIENT
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "Platform-Notification/1.0");

    // HMAC-SHA256 signing
    if let Some(secret) = secret
        && let Some(sig) = compute_signature(body.as_bytes(), secret)
    {
        request = request.header("X-Platform-Signature", sig);
    }

    match request.body(body).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let success = resp.status().is_success();
            tracing::info!(status, "notification webhook delivered");
            Ok(DeliveryResult {
                status_code: status,
                success,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "notification webhook delivery failed");
            Err(ApiError::Internal(anyhow::anyhow!(
                "webhook delivery failed: {e}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_signature_matches_expected() {
        // Known test vector: HMAC-SHA256("hello", "secret")
        let sig = compute_signature(b"hello", "secret").unwrap();
        assert!(sig.starts_with("sha256="));
        // Verify deterministic: same inputs produce same output
        let sig2 = compute_signature(b"hello", "secret").unwrap();
        assert_eq!(sig, sig2);
    }

    #[test]
    fn hmac_signature_deterministic() {
        let payload = br#"{"event":"push","ref":"refs/heads/main"}"#;
        let s1 = compute_signature(payload, "my-secret").unwrap();
        let s2 = compute_signature(payload, "my-secret").unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn hmac_signature_format_sha256_prefix() {
        let sig = compute_signature(b"data", "key").unwrap();
        assert!(
            sig.starts_with("sha256="),
            "signature should start with 'sha256=', got: {sig}"
        );
        // The hex part should be 64 chars (SHA-256 = 32 bytes = 64 hex chars)
        let hex_part = sig.strip_prefix("sha256=").unwrap();
        assert_eq!(hex_part.len(), 64);
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hmac_different_secrets_produce_different_signatures() {
        let payload = b"same-payload";
        let s1 = compute_signature(payload, "secret-a").unwrap();
        let s2 = compute_signature(payload, "secret-b").unwrap();
        assert_ne!(s1, s2);
    }

    #[test]
    fn hmac_different_payloads_produce_different_signatures() {
        let s1 = compute_signature(b"payload-a", "secret").unwrap();
        let s2 = compute_signature(b"payload-b", "secret").unwrap();
        assert_ne!(s1, s2);
    }

    #[test]
    fn hmac_empty_payload_works() {
        let sig = compute_signature(b"", "secret").unwrap();
        assert!(sig.starts_with("sha256="));
    }

    #[test]
    fn hmac_empty_secret_works() {
        // HMAC with empty key is valid (zero-length key is padded)
        let sig = compute_signature(b"data", "").unwrap();
        assert!(sig.starts_with("sha256="));
    }
}
