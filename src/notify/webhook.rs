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

/// Deliver a JSON payload to a webhook URL with optional HMAC-SHA256 signing.
///
/// Reuses the shared webhook HTTP client and SSRF protection from `api::webhooks`.
#[tracing::instrument(skip(payload, secret), err)]
pub async fn deliver(
    url: &str,
    payload: &serde_json::Value,
    secret: Option<&str>,
) -> Result<DeliveryResult, ApiError> {
    // SSRF protection
    validate_webhook_url(url)?;

    // Acquire concurrency permit
    let _permit = WEBHOOK_SEMAPHORE.try_acquire().map_err(|_| {
        tracing::warn!(
            url,
            "notification webhook dropped: concurrency limit reached"
        );
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
        && let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
    {
        mac.update(body.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        request = request.header("X-Platform-Signature", format!("sha256={signature}"));
    }

    match request.body(body).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let success = resp.status().is_success();
            tracing::info!(url, status, "notification webhook delivered");
            Ok(DeliveryResult {
                status_code: status,
                success,
            })
        }
        Err(e) => {
            tracing::warn!(url, error = %e, "notification webhook delivery failed");
            Err(ApiError::Internal(anyhow::anyhow!(
                "webhook delivery failed: {e}"
            )))
        }
    }
}
