use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub status: Option<String>,
    #[serde(rename = "type")]
    pub notification_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

#[derive(Debug, Serialize)]
pub struct NotificationResponse {
    pub id: Uuid,
    pub notification_type: String,
    pub subject: String,
    pub body: Option<String>,
    pub channel: String,
    pub status: String,
    pub ref_type: Option<String>,
    pub ref_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct UnreadCountResponse {
    pub count: i64,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/unread-count", get(unread_count))
        .route(
            "/api/notifications/{id}/read",
            axum::routing::patch(mark_read),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_notifications(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<NotificationResponse>>, ApiError> {
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!"
        FROM notifications
        WHERE user_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::text IS NULL OR notification_type = $3)
        "#,
        auth.user_id,
        params.status,
        params.notification_type,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, notification_type, subject, body, channel, status,
               ref_type, ref_id, created_at
        FROM notifications
        WHERE user_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::text IS NULL OR notification_type = $3)
        ORDER BY created_at DESC
        LIMIT $4 OFFSET $5
        "#,
        auth.user_id,
        params.status,
        params.notification_type,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| NotificationResponse {
            id: r.id,
            notification_type: r.notification_type,
            subject: r.subject,
            body: r.body,
            channel: r.channel,
            status: r.status,
            ref_type: r.ref_type,
            ref_id: r.ref_id,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn unread_count(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<UnreadCountResponse>, ApiError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!"
        FROM notifications
        WHERE user_id = $1 AND status IN ('pending', 'sent')
        "#,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(UnreadCountResponse { count }))
}

async fn mark_read(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result = sqlx::query!(
        r#"
        UPDATE notifications SET status = 'read'
        WHERE id = $1 AND user_id = $2 AND status != 'read'
        "#,
        id,
        auth.user_id,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        // Either doesn't exist or doesn't belong to user — return 404
        return Err(ApiError::NotFound("notification".into()));
    }

    Ok(Json(serde_json::json!({"ok": true})))
}
