// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::PlatformState;
use platform_git::gpg_keys;
use platform_types::{ApiError, AuditEntry, AuthUser, ListResponse, send_audit, validation};

use super::helpers::require_admin;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AddGpgKeyRequest {
    pub public_key: String,
}

#[derive(Debug, Serialize)]
pub struct GpgKeyResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub key_id: String,
    pub fingerprint: String,
    pub emails: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub can_sign: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct GpgKeyDetailResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub key_id: String,
    pub fingerprint: String,
    pub public_key_armor: String,
    pub emails: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub can_sign: bool,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route(
            "/api/users/me/gpg-keys",
            get(list_gpg_keys).post(add_gpg_key),
        )
        .route(
            "/api/users/me/gpg-keys/{id}",
            get(get_gpg_key).delete(delete_gpg_key),
        )
        .route(
            "/api/admin/users/{user_id}/gpg-keys",
            get(admin_list_gpg_keys),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/users/me/gpg-keys
async fn add_gpg_key(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<AddGpgKeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validation::check_length("public_key", &body.public_key, 100, 100_000)?;

    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "gpg_key_add",
        &auth.user_id.to_string(),
        20,
        300,
    )
    .await?;

    // Parse the GPG key in a blocking task (CPU-intensive)
    let armor = body.public_key.clone();
    let parsed = tokio::task::spawn_blocking(move || gpg_keys::parse_gpg_public_key(&armor))
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Get user email for verification
    let user_email: String =
        sqlx::query_scalar!("SELECT email FROM users WHERE id = $1", auth.user_id,)
            .fetch_one(&state.pool)
            .await?;

    // At least one UID email must match the user's platform email
    if !gpg_keys::verify_email_match(&parsed.emails, &user_email) {
        return Err(ApiError::BadRequest(
            "GPG key must contain a UID email matching your account email".into(),
        ));
    }

    // Enforce max 50 keys per user
    let count: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM user_gpg_keys WHERE user_id = $1"#,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;

    if count >= 50 {
        return Err(ApiError::BadRequest(
            "maximum of 50 GPG keys per user".into(),
        ));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();

    // Insert — fingerprint UNIQUE constraint catches duplicates (→ 409)
    sqlx::query!(
        r#"INSERT INTO user_gpg_keys (id, user_id, key_id, fingerprint, public_key_armor, public_key_bytes, emails, expires_at, can_sign, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
        id,
        auth.user_id,
        parsed.key_id,
        parsed.fingerprint,
        parsed.public_key_armor,
        parsed.public_key_bytes,
        &parsed.emails,
        parsed.expires_at,
        parsed.can_sign,
        now,
    )
    .execute(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "gpg_key.add".into(),
            resource: "gpg_key".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({
                "key_id": parsed.key_id,
                "fingerprint": parsed.fingerprint,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    let resp = GpgKeyResponse {
        id,
        user_id: auth.user_id,
        key_id: parsed.key_id,
        fingerprint: parsed.fingerprint,
        emails: parsed.emails,
        expires_at: parsed.expires_at,
        can_sign: parsed.can_sign,
        created_at: now,
    };

    Ok((StatusCode::CREATED, Json(resp)))
}

/// GET /api/users/me/gpg-keys
async fn list_gpg_keys(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<ListResponse<GpgKeyResponse>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT id, user_id, key_id, fingerprint, emails, expires_at, can_sign, created_at
           FROM user_gpg_keys
           WHERE user_id = $1
           ORDER BY created_at DESC"#,
        auth.user_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let keys: Vec<GpgKeyResponse> = rows
        .into_iter()
        .map(|r| GpgKeyResponse {
            id: r.id,
            user_id: r.user_id,
            key_id: r.key_id,
            fingerprint: r.fingerprint,
            emails: r.emails,
            expires_at: r.expires_at,
            can_sign: r.can_sign,
            created_at: r.created_at,
        })
        .collect();

    let total = i64::try_from(keys.len()).unwrap_or(0);
    Ok(Json(ListResponse { items: keys, total }))
}

/// GET /api/users/me/gpg-keys/{id}
async fn get_gpg_key(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<GpgKeyDetailResponse>, ApiError> {
    let row = sqlx::query!(
        r#"SELECT id, user_id, key_id, fingerprint, public_key_armor, emails, expires_at, can_sign, created_at
           FROM user_gpg_keys
           WHERE id = $1 AND user_id = $2"#,
        id,
        auth.user_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("gpg key".into()))?;

    Ok(Json(GpgKeyDetailResponse {
        id: row.id,
        user_id: row.user_id,
        key_id: row.key_id,
        fingerprint: row.fingerprint,
        public_key_armor: row.public_key_armor,
        emails: row.emails,
        expires_at: row.expires_at,
        can_sign: row.can_sign,
        created_at: row.created_at,
    }))
}

/// DELETE /api/users/me/gpg-keys/{id}
async fn delete_gpg_key(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let result = sqlx::query!(
        "DELETE FROM user_gpg_keys WHERE id = $1 AND user_id = $2 RETURNING fingerprint",
        id,
        auth.user_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    let row = result.ok_or_else(|| ApiError::NotFound("gpg key".into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "gpg_key.delete".into(),
            resource: "gpg_key".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({
                "fingerprint": row.fingerprint,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/admin/users/{user_id}/gpg-keys
async fn admin_list_gpg_keys(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(user_id): Path<Uuid>,
) -> Result<Json<ListResponse<GpgKeyResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let rows = sqlx::query!(
        r#"SELECT id, user_id, key_id, fingerprint, emails, expires_at, can_sign, created_at
           FROM user_gpg_keys
           WHERE user_id = $1
           ORDER BY created_at DESC"#,
        user_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let keys: Vec<GpgKeyResponse> = rows
        .into_iter()
        .map(|r| GpgKeyResponse {
            id: r.id,
            user_id: r.user_id,
            key_id: r.key_id,
            fingerprint: r.fingerprint,
            emails: r.emails,
            expires_at: r.expires_at,
            can_sign: r.can_sign,
            created_at: r.created_at,
        })
        .collect();

    let total = i64::try_from(keys.len()).unwrap_or(0);
    Ok(Json(ListResponse { items: keys, total }))
}
