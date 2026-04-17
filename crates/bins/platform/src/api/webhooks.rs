// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use std::net::IpAddr;
use std::sync::LazyLock;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use uuid::Uuid;

use platform_types::{ApiError, AuditEntry, AuthUser, send_audit, validation};

use super::helpers::require_project_write;
use crate::state::PlatformState;

/// Shared HTTP client for webhook dispatch (with timeouts).
#[allow(dead_code)]
pub(crate) static WEBHOOK_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build webhook HTTP client")
});

/// Concurrency limiter for webhook dispatch (max 50 concurrent deliveries).
#[allow(dead_code)]
pub(crate) static WEBHOOK_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(50));

// ---------------------------------------------------------------------------
// SSRF protection
// ---------------------------------------------------------------------------

/// Validate a webhook URL to block SSRF attacks.
/// Rejects private/loopback IPs, link-local, metadata endpoints, and non-HTTP schemes.
#[allow(dead_code)]
pub(crate) fn validate_webhook_url(url_str: &str) -> Result<(), ApiError> {
    let parsed =
        url::Url::parse(url_str).map_err(|_| ApiError::BadRequest("invalid URL".into()))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::BadRequest(
            "webhook URL must use http or https".into(),
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::BadRequest("webhook URL must have a host".into()))?;

    // Block well-known dangerous hostnames
    let blocked_hosts = [
        "localhost",
        "169.254.169.254",
        "metadata.google.internal",
        "[::1]",
    ];
    let host_lower = host.to_lowercase();
    if blocked_hosts.iter().any(|b| host_lower == *b) {
        return Err(ApiError::BadRequest(
            "webhook URL must not target internal/metadata endpoints".into(),
        ));
    }

    // Block private/reserved IPs (strip brackets for IPv6 literals like [::1])
    let bare_ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare_ip.parse::<IpAddr>()
        && validation::is_private_ip(ip)
    {
        return Err(ApiError::BadRequest(
            "webhook URL must not target private/reserved IP addresses".into(),
        ));
    }

    Ok(())
}
// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateWebhookRequest {
    pub url: String,
    pub events: Vec<String>,
    pub secret: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateWebhookRequest {
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    pub secret: Option<String>,
    pub active: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct WebhookResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub url: String,
    pub events: Vec<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route(
            "/api/projects/{id}/webhooks",
            get(list_webhooks).post(create_webhook),
        )
        .route(
            "/api/projects/{id}/webhooks/{wh_id}",
            get(get_webhook)
                .patch(update_webhook)
                .delete(delete_webhook),
        )
        .route(
            "/api/projects/{id}/webhooks/{wh_id}/test",
            axum::routing::post(test_webhook),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_webhook(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_write(&state, &auth, id).await?;

    // Verify project is active (soft-delete check)
    let active: bool = sqlx::query_scalar("SELECT is_active FROM projects WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or(false);
    if !active {
        return Err(ApiError::NotFound("project".into()));
    }

    // Validate URL (format + SSRF protection)
    validation::check_url(&body.url)?;
    validation::check_ssrf_url(&body.url, &["http", "https"])?;

    // Validate secret length
    if let Some(ref secret) = body.secret {
        validation::check_length("secret", secret, 0, 1024)?;
    }

    // Validate events
    if body.events.is_empty() {
        return Err(ApiError::BadRequest("events must not be empty".into()));
    }
    if body.events.len() > 20 {
        return Err(ApiError::BadRequest("max 20 events".into()));
    }
    let valid_events = ["push", "mr", "issue", "build", "deploy", "agent"];
    for event in &body.events {
        if !valid_events.contains(&event.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "invalid event '{event}'; valid events: {valid_events:?}"
            )));
        }
    }

    let wh = sqlx::query!(
        r#"
        INSERT INTO webhooks (project_id, url, events, secret)
        VALUES ($1, $2, $3, $4)
        RETURNING id, project_id, url, events, active, created_at
        "#,
        id,
        body.url,
        &body.events,
        body.secret,
    )
    .fetch_one(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "webhook.create".into(),
            resource: "webhook".into(),
            resource_id: Some(wh.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"events": body.events})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((
        StatusCode::CREATED,
        Json(WebhookResponse {
            id: wh.id,
            project_id: wh.project_id,
            url: wh.url,
            events: wh.events,
            active: wh.active,
            created_at: wh.created_at,
        }),
    ))
}

async fn list_webhooks(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<platform_types::ListResponse<WebhookResponse>>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM webhooks WHERE project_id = $1"#,
        id,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, url, events, active, created_at
        FROM webhooks WHERE project_id = $1
        ORDER BY created_at DESC
        "#,
        id,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|w| WebhookResponse {
            id: w.id,
            project_id: w.project_id,
            url: w.url,
            events: w.events,
            active: w.active,
            created_at: w.created_at,
        })
        .collect();

    Ok(Json(platform_types::ListResponse { items, total }))
}

async fn get_webhook(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, wh_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<WebhookResponse>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let wh = sqlx::query!(
        r#"
        SELECT id, project_id, url, events, active, created_at
        FROM webhooks WHERE id = $1 AND project_id = $2
        "#,
        wh_id,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("webhook".into()))?;

    Ok(Json(WebhookResponse {
        id: wh.id,
        project_id: wh.project_id,
        url: wh.url,
        events: wh.events,
        active: wh.active,
        created_at: wh.created_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %wh_id), err)]
async fn update_webhook(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, wh_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateWebhookRequest>,
) -> Result<Json<WebhookResponse>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    // Validate inputs
    if let Some(ref url) = body.url {
        validation::check_url(url)?;
        validation::check_ssrf_url(url, &["http", "https"])?;
    }
    if let Some(ref secret) = body.secret {
        validation::check_length("secret", secret, 0, 1024)?;
    }
    if let Some(ref events) = body.events {
        if events.is_empty() {
            return Err(ApiError::BadRequest("events must not be empty".into()));
        }
        if events.len() > 20 {
            return Err(ApiError::BadRequest("max 20 events".into()));
        }
        let valid_events = ["push", "mr", "issue", "build", "deploy", "agent"];
        for event in events {
            if !valid_events.contains(&event.as_str()) {
                return Err(ApiError::BadRequest(format!(
                    "invalid event '{event}'; valid events: {valid_events:?}"
                )));
            }
        }
    }

    let wh = sqlx::query!(
        r#"
        UPDATE webhooks SET
            url = COALESCE($3, url),
            events = COALESCE($4, events),
            secret = COALESCE($5, secret),
            active = COALESCE($6, active)
        WHERE id = $1 AND project_id = $2
        RETURNING id, project_id, url, events, active, created_at
        "#,
        wh_id,
        id,
        body.url,
        body.events.as_deref(),
        body.secret,
        body.active,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("webhook".into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "webhook.update".into(),
            resource: "webhook".into(),
            resource_id: Some(wh_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(WebhookResponse {
        id: wh.id,
        project_id: wh.project_id,
        url: wh.url,
        events: wh.events,
        active: wh.active,
        created_at: wh.created_at,
    }))
}

#[tracing::instrument(skip(state), fields(%id, %wh_id), err)]
async fn delete_webhook(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, wh_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let result = sqlx::query!(
        "DELETE FROM webhooks WHERE id = $1 AND project_id = $2",
        wh_id,
        id,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("webhook".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "webhook.delete".into(),
            resource: "webhook".into(),
            resource_id: Some(wh_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(skip(state), fields(%id, %wh_id), err)]
async fn test_webhook(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, wh_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let wh = sqlx::query!(
        "SELECT url, secret FROM webhooks WHERE id = $1 AND project_id = $2",
        wh_id,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("webhook".into()))?;

    let payload = serde_json::json!({
        "event": "test",
        "project_id": id,
        "message": "webhook test delivery",
    });

    dispatch_single(
        wh_id,
        &wh.url,
        wh.secret.as_deref(),
        &payload,
        &state.webhook_semaphore,
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Webhook dispatch (shared utility)
// ---------------------------------------------------------------------------

/// Fire all active webhooks for a project + event.
/// Spawns background tasks for each delivery.
pub async fn fire_webhooks(
    pool: &PgPool,
    project_id: Uuid,
    event: &str,
    payload: &serde_json::Value,
    semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
) {
    let webhooks = match sqlx::query!(
        r#"
        SELECT id, url, secret
        FROM webhooks
        WHERE project_id = $1 AND active = true AND $2 = ANY(events)
        "#,
        project_id,
        event,
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, project_id = %project_id, event, "failed to query webhooks");
            return;
        }
    };

    for wh in webhooks {
        let webhook_id = wh.id;
        let url = wh.url.clone();
        let secret = wh.secret.clone();
        let payload = payload.clone();
        let sem = semaphore.clone();

        tokio::spawn(async move {
            dispatch_single(webhook_id, &url, secret.as_deref(), &payload, &sem).await;
        });
    }
}

pub(crate) async fn dispatch_single(
    webhook_id: Uuid,
    url: &str,
    secret: Option<&str>,
    payload: &serde_json::Value,
    semaphore: &tokio::sync::Semaphore,
) {
    // S63: Re-validate SSRF before dispatch — URL may have been modified in DB
    // Skip in dev mode to allow localhost URLs (e.g., test wiremock servers)
    static DEV_MODE: LazyLock<bool> = LazyLock::new(|| {
        std::env::var("PLATFORM_DEV")
            .ok()
            .is_some_and(|v| v == "true")
    });
    if !*DEV_MODE && validation::check_ssrf_url(url, &["http", "https"]).is_err() {
        tracing::warn!(webhook_id = %webhook_id, "webhook URL failed SSRF re-validation, skipping dispatch");
        return;
    }

    // Acquire semaphore permit (concurrency limit)
    let Ok(_permit) = semaphore.try_acquire() else {
        tracing::warn!(webhook_id = %webhook_id, "webhook dispatch dropped: concurrency limit reached");
        return;
    };

    let body = match serde_json::to_string(payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, webhook_id = %webhook_id, "failed to serialize webhook payload");
            return;
        }
    };

    let mut request = WEBHOOK_CLIENT
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "Platform-Webhook/1.0");

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
            tracing::info!(webhook_id = %webhook_id, status = resp.status().as_u16(), "webhook delivered");
        }
        Err(e) => {
            tracing::warn!(webhook_id = %webhook_id, error = %e, "webhook delivery failed");
        }
    }
}

// SSRF and IP validation tests moved to src/validation.rs

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_webhook_url tests --

    #[test]
    fn valid_https_url() {
        assert!(validate_webhook_url("https://example.com/webhook").is_ok());
    }

    #[test]
    fn valid_http_url() {
        assert!(validate_webhook_url("http://example.com/webhook").is_ok());
    }

    #[test]
    fn rejects_ftp_scheme() {
        assert!(validate_webhook_url("ftp://example.com/file").is_err());
    }

    #[test]
    fn rejects_file_scheme() {
        assert!(validate_webhook_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn rejects_javascript_scheme() {
        assert!(validate_webhook_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn rejects_localhost() {
        assert!(validate_webhook_url("http://localhost/webhook").is_err());
    }

    #[test]
    fn rejects_metadata_endpoint() {
        assert!(validate_webhook_url("http://169.254.169.254/latest/meta-data").is_err());
    }

    #[test]
    fn rejects_google_metadata() {
        assert!(
            validate_webhook_url("http://metadata.google.internal/computeMetadata/v1/").is_err()
        );
    }

    #[test]
    fn rejects_ipv6_loopback() {
        assert!(validate_webhook_url("http://[::1]/webhook").is_err());
    }

    #[test]
    fn rejects_private_ip_10() {
        assert!(validate_webhook_url("http://10.0.0.1/webhook").is_err());
    }

    #[test]
    fn rejects_private_ip_172() {
        assert!(validate_webhook_url("http://172.16.0.1/webhook").is_err());
    }

    #[test]
    fn rejects_private_ip_192() {
        assert!(validate_webhook_url("http://192.168.1.1/webhook").is_err());
    }

    #[test]
    fn rejects_loopback_127() {
        assert!(validate_webhook_url("http://127.0.0.1/webhook").is_err());
    }

    #[test]
    fn rejects_invalid_url() {
        assert!(validate_webhook_url("not a url").is_err());
    }

    #[test]
    fn rejects_empty_url() {
        assert!(validate_webhook_url("").is_err());
    }

    #[test]
    fn allows_public_ip() {
        assert!(validate_webhook_url("http://8.8.8.8/webhook").is_ok());
    }

    #[test]
    fn allows_public_domain() {
        assert!(validate_webhook_url("https://hooks.slack.com/services/T00000000").is_ok());
    }

    #[test]
    fn localhost_case_insensitive() {
        assert!(validate_webhook_url("http://LOCALHOST/webhook").is_err());
        assert!(validate_webhook_url("http://Localhost/webhook").is_err());
    }

    // -- HMAC-SHA256 signing tests --

    #[test]
    fn hmac_signature_is_deterministic() {
        let secret = "my-webhook-secret";
        let payload = r#"{"event":"test"}"#;

        let mut mac1 = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac1.update(payload.as_bytes());
        let sig1 = hex::encode(mac1.finalize().into_bytes());

        let mut mac2 = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac2.update(payload.as_bytes());
        let sig2 = hex::encode(mac2.finalize().into_bytes());

        assert_eq!(sig1, sig2);
        assert!(sig1.starts_with(|c: char| c.is_ascii_hexdigit()));
        assert_eq!(sig1.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn hmac_different_secrets_produce_different_signatures() {
        let payload = r#"{"event":"test"}"#;

        let mut mac1 = Hmac::<Sha256>::new_from_slice(b"secret-1").unwrap();
        mac1.update(payload.as_bytes());
        let sig1 = hex::encode(mac1.finalize().into_bytes());

        let mut mac2 = Hmac::<Sha256>::new_from_slice(b"secret-2").unwrap();
        mac2.update(payload.as_bytes());
        let sig2 = hex::encode(mac2.finalize().into_bytes());

        assert_ne!(sig1, sig2);
    }

    #[test]
    fn hmac_different_payloads_produce_different_signatures() {
        let secret = b"my-secret";

        let mut mac1 = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac1.update(b"payload-1");
        let sig1 = hex::encode(mac1.finalize().into_bytes());

        let mut mac2 = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac2.update(b"payload-2");
        let sig2 = hex::encode(mac2.finalize().into_bytes());

        assert_ne!(sig1, sig2);
    }

    // -- webhook_client + semaphore config --

    #[test]
    fn webhook_semaphore_is_initialized() {
        // Verify the semaphore is accessible (LazyLock is initialized)
        // We can't test exact capacity since it's shared across tests,
        // but we verify it exists and can be used.
        let permit = WEBHOOK_SEMAPHORE.try_acquire();
        assert!(permit.is_ok(), "semaphore should have available permits");
        // Drop permit to release it for other tests
    }

    #[test]
    fn webhook_client_is_initialized() {
        // Verify the client LazyLock initializes without panic
        let _client = &*WEBHOOK_CLIENT;
    }
}
