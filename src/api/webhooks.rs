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

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::validation;

/// Shared HTTP client for webhook dispatch (with timeouts).
static WEBHOOK_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build webhook HTTP client")
});

/// Concurrency limiter for webhook dispatch (max 50 concurrent deliveries).
static WEBHOOK_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(50));

async fn require_project_write(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectWrite,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
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

pub fn router() -> Router<AppState> {
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
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_write(&state, &auth, id).await?;

    // Validate URL (format + SSRF protection)
    validation::check_url(&body.url)?;
    validation::check_ssrf_url(&body.url, &["http", "https"])?;

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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "webhook.create",
            resource: "webhook",
            resource_id: Some(wh.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"events": body.events})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

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
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<WebhookResponse>>, ApiError> {
    require_project_write(&state, &auth, id).await?;

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

    Ok(Json(items))
}

async fn get_webhook(
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "webhook.update",
            resource: "webhook",
            resource_id: Some(wh_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

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
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, wh_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "webhook.delete",
            resource: "webhook",
            resource_id: Some(wh_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

#[tracing::instrument(skip(state), fields(%id, %wh_id), err)]
async fn test_webhook(
    State(state): State<AppState>,
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

    dispatch_single(&wh.url, wh.secret.as_deref(), &payload).await;

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
) {
    let webhooks = match sqlx::query!(
        r#"
        SELECT url, secret
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
        let url = wh.url.clone();
        let secret = wh.secret.clone();
        let payload = payload.clone();

        tokio::spawn(async move {
            dispatch_single(&url, secret.as_deref(), &payload).await;
        });
    }
}

async fn dispatch_single(url: &str, secret: Option<&str>, payload: &serde_json::Value) {
    // Acquire semaphore permit (concurrency limit)
    let Ok(_permit) = WEBHOOK_SEMAPHORE.try_acquire() else {
        tracing::warn!(url, "webhook dispatch dropped: concurrency limit reached");
        return;
    };

    let body = match serde_json::to_string(payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, url, "failed to serialize webhook payload");
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
            tracing::info!(url, status = resp.status().as_u16(), "webhook delivered");
        }
        Err(e) => {
            tracing::warn!(url, error = %e, "webhook delivery failed");
        }
    }
}

// SSRF and IP validation tests moved to src/validation.rs
