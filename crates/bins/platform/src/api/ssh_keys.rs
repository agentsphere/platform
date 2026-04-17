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
use sqlx::Row;
use uuid::Uuid;

use crate::state::PlatformState;
use platform_git::ssh_keys;
use platform_types::{ApiError, AuditEntry, AuthUser, ListResponse, send_audit, validation};

use super::helpers::require_admin;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AddSshKeyRequest {
    pub name: String,
    pub public_key: String,
}

#[derive(Debug, Serialize)]
pub struct SshKeyResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub algorithm: String,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route(
            "/api/users/me/ssh-keys",
            get(list_ssh_keys).post(add_ssh_key),
        )
        .route(
            "/api/users/me/ssh-keys/{id}",
            get(get_ssh_key).delete(delete_ssh_key),
        )
        .route(
            "/api/admin/users/{user_id}/ssh-keys",
            get(admin_list_ssh_keys),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/users/me/ssh-keys
async fn add_ssh_key(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<AddSshKeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validation::check_length("name", &body.name, 1, 255)?;
    validation::check_length("public_key", &body.public_key, 20, 16384)?;

    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "ssh_key_add",
        &auth.user_id.to_string(),
        20,
        300,
    )
    .await?;

    let parsed = ssh_keys::parse_ssh_public_key(&body.public_key)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Enforce max 50 keys per user
    let count: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM user_ssh_keys WHERE user_id = $1"#,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;

    if count >= 50 {
        return Err(ApiError::BadRequest(
            "maximum of 50 SSH keys per user".into(),
        ));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();

    // Insert — fingerprint UNIQUE constraint will catch duplicates (→ 409 via sqlx error mapping)
    sqlx::query!(
        r#"INSERT INTO user_ssh_keys (id, user_id, name, algorithm, fingerprint, public_key_openssh, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
        id,
        auth.user_id,
        body.name,
        parsed.algorithm,
        parsed.fingerprint,
        parsed.public_key_openssh,
        now,
    )
    .execute(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "ssh_key.add".into(),
            resource: "ssh_key".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({
                "name": body.name,
                "algorithm": parsed.algorithm,
                "fingerprint": parsed.fingerprint,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    let resp = SshKeyResponse {
        id,
        user_id: auth.user_id,
        name: body.name,
        algorithm: parsed.algorithm,
        fingerprint: parsed.fingerprint,
        last_used_at: None,
        created_at: now,
    };

    Ok((StatusCode::CREATED, Json(resp)))
}

/// GET /api/users/me/ssh-keys
async fn list_ssh_keys(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<ListResponse<SshKeyResponse>>, ApiError> {
    let keys = sqlx::query_as!(
        SshKeyResponse,
        r#"SELECT id, user_id, name, algorithm, fingerprint,
                  last_used_at, created_at
           FROM user_ssh_keys
           WHERE user_id = $1
           ORDER BY created_at DESC"#,
        auth.user_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let total = i64::try_from(keys.len()).unwrap_or(0);
    Ok(Json(ListResponse { items: keys, total }))
}

/// GET /api/users/me/ssh-keys/{id}
async fn get_ssh_key(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<SshKeyResponse>, ApiError> {
    let row = sqlx::query(
        "SELECT id, user_id, name, algorithm, fingerprint, last_used_at, created_at \
         FROM user_ssh_keys WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(auth.user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("ssh key".into()))?;

    Ok(Json(SshKeyResponse {
        id: row.get("id"),
        user_id: row.get("user_id"),
        name: row.get("name"),
        algorithm: row.get("algorithm"),
        fingerprint: row.get("fingerprint"),
        last_used_at: row.get("last_used_at"),
        created_at: row.get("created_at"),
    }))
}

/// DELETE /api/users/me/ssh-keys/{id}
async fn delete_ssh_key(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let result = sqlx::query!(
        "DELETE FROM user_ssh_keys WHERE id = $1 AND user_id = $2 RETURNING fingerprint",
        id,
        auth.user_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    let row = result.ok_or_else(|| ApiError::NotFound("ssh key".into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "ssh_key.delete".into(),
            resource: "ssh_key".into(),
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

/// GET /api/admin/users/{user_id}/ssh-keys
async fn admin_list_ssh_keys(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(user_id): Path<Uuid>,
) -> Result<Json<ListResponse<SshKeyResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let keys = sqlx::query_as!(
        SshKeyResponse,
        r#"SELECT id, user_id, name, algorithm, fingerprint,
                  last_used_at, created_at
           FROM user_ssh_keys
           WHERE user_id = $1
           ORDER BY created_at DESC"#,
        user_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let total = i64::try_from(keys.len()).unwrap_or(0);
    Ok(Json(ListResponse { items: keys, total }))
}
