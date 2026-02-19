use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;
use crate::rbac::resolver;
use crate::rbac::types::Permission;

/// Parameters for creating a new delegation (internal, not the API request type).
#[derive(Debug)]
pub struct CreateDelegationParams {
    pub delegator_id: Uuid,
    pub delegate_id: Uuid,
    pub permission: Permission,
    pub project_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct Delegation {
    pub id: Uuid,
    pub delegator_id: Uuid,
    pub delegate_id: Uuid,
    pub permission_id: Uuid,
    pub permission_name: String,
    pub project_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Create a new delegation. Validates that the delegator holds the permission.
#[tracing::instrument(skip(pool, valkey, req), fields(delegator_id = %req.delegator_id, delegate_id = %req.delegate_id, permission = %req.permission), err)]
pub async fn create_delegation(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    req: &CreateDelegationParams,
) -> Result<Delegation, ApiError> {
    // Validate delegator holds this permission
    let delegator_has = resolver::has_permission(
        pool,
        valkey,
        req.delegator_id,
        req.project_id,
        req.permission,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !delegator_has {
        return Err(ApiError::Forbidden);
    }

    // Look up the permission ID from the name
    let permission_id = sqlx::query_scalar!(
        "SELECT id FROM permissions WHERE name = $1",
        req.permission.as_str(),
    )
    .fetch_one(pool)
    .await?;

    let id = Uuid::new_v4();
    let now = Utc::now();

    sqlx::query!(
        r#"
        INSERT INTO delegations (id, delegator_id, delegate_id, permission_id, project_id, expires_at, reason)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        id,
        req.delegator_id,
        req.delegate_id,
        permission_id,
        req.project_id,
        req.expires_at,
        req.reason,
    )
    .execute(pool)
    .await?;

    // Invalidate delegate's cached permissions
    let _ = resolver::invalidate_permissions(valkey, req.delegate_id, req.project_id).await;

    Ok(Delegation {
        id,
        delegator_id: req.delegator_id,
        delegate_id: req.delegate_id,
        permission_id,
        permission_name: req.permission.as_str().to_owned(),
        project_id: req.project_id,
        expires_at: req.expires_at,
        reason: req.reason.clone(),
        created_at: now,
        revoked_at: None,
    })
}

/// Revoke a delegation by setting `revoked_at`. Returns the delegate's `user_id` for cache invalidation.
#[tracing::instrument(skip(pool, valkey), fields(%delegation_id), err)]
pub async fn revoke_delegation(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    delegation_id: Uuid,
) -> Result<(), ApiError> {
    let row = sqlx::query!(
        r#"
        UPDATE delegations SET revoked_at = now()
        WHERE id = $1 AND revoked_at IS NULL
        RETURNING delegate_id, project_id
        "#,
        delegation_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("delegation".into()))?;

    // Invalidate delegate's cached permissions
    let _ = resolver::invalidate_permissions(valkey, row.delegate_id, row.project_id).await;

    Ok(())
}

/// List delegations for a user (both granted by and received).
#[tracing::instrument(skip(pool), fields(%user_id), err)]
pub async fn list_delegations(pool: &PgPool, user_id: Uuid) -> Result<Vec<Delegation>, ApiError> {
    let rows = sqlx::query_as!(
        Delegation,
        r#"
        SELECT
            d.id,
            d.delegator_id,
            d.delegate_id,
            d.permission_id,
            p.name as "permission_name!",
            d.project_id,
            d.expires_at,
            d.reason,
            d.created_at,
            d.revoked_at
        FROM delegations d
        JOIN permissions p ON p.id = d.permission_id
        WHERE d.delegator_id = $1 OR d.delegate_id = $1
        ORDER BY d.created_at DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}
