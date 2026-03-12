use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ts_rs::TS;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::store::AppState;
use crate::validation;

use super::helpers::{ListResponse, require_project_write};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Deserialize)]
pub struct CreateProtectionRequest {
    pub pattern: String,
    #[serde(default = "default_true")]
    pub require_pr: bool,
    #[serde(default = "default_true")]
    pub block_force_push: bool,
    #[serde(default)]
    pub required_approvals: i32,
    #[serde(default = "default_true")]
    pub dismiss_stale_reviews: bool,
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default)]
    pub require_up_to_date: bool,
    #[serde(default)]
    pub allow_admin_bypass: bool,
    #[serde(default = "default_merge_methods")]
    pub merge_methods: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_merge_methods() -> Vec<String> {
    vec!["merge".to_string()]
}

#[derive(Debug, Deserialize)]
pub struct UpdateProtectionRequest {
    pub require_pr: Option<bool>,
    pub block_force_push: Option<bool>,
    pub required_approvals: Option<i32>,
    pub dismiss_stale_reviews: Option<bool>,
    pub required_checks: Option<Vec<String>>,
    pub require_up_to_date: Option<bool>,
    pub allow_admin_bypass: Option<bool>,
    pub merge_methods: Option<Vec<String>>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "BranchProtection")]
pub struct ProtectionResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub pattern: String,
    pub require_pr: bool,
    pub block_force_push: bool,
    pub required_approvals: i32,
    pub dismiss_stale_reviews: bool,
    pub required_checks: Vec<String>,
    pub require_up_to_date: bool,
    pub allow_admin_bypass: bool,
    pub merge_methods: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/branch-protections",
            get(list_protections).post(create_protection),
        )
        .route(
            "/api/projects/{id}/branch-protections/{rule_id}",
            get(get_protection)
                .patch(update_protection)
                .delete(delete_protection),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_protections(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ListResponse<ProtectionResponse>>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, pattern, require_pr, block_force_push,
               required_approvals, dismiss_stale_reviews, required_checks,
               require_up_to_date, allow_admin_bypass, merge_methods,
               created_at, updated_at
        FROM branch_protection_rules
        WHERE project_id = $1
        ORDER BY created_at ASC
        "#,
        id,
    )
    .fetch_all(&state.pool)
    .await?;

    let total = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let items = rows
        .into_iter()
        .map(|r| ProtectionResponse {
            id: r.id,
            project_id: r.project_id,
            pattern: r.pattern,
            require_pr: r.require_pr,
            block_force_push: r.block_force_push,
            required_approvals: r.required_approvals,
            dismiss_stale_reviews: r.dismiss_stale_reviews,
            required_checks: r.required_checks,
            require_up_to_date: r.require_up_to_date,
            allow_admin_bypass: r.allow_admin_bypass,
            merge_methods: r.merge_methods,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_protection(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateProtectionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_write(&state, &auth, id).await?;
    validate_protection_inputs(&body.pattern, &body.merge_methods, body.required_approvals)?;

    let row = sqlx::query!(
        r#"
        INSERT INTO branch_protection_rules
            (project_id, pattern, require_pr, block_force_push, required_approvals,
             dismiss_stale_reviews, required_checks, require_up_to_date,
             allow_admin_bypass, merge_methods)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING id, project_id, pattern, require_pr, block_force_push,
                  required_approvals, dismiss_stale_reviews, required_checks,
                  require_up_to_date, allow_admin_bypass, merge_methods,
                  created_at, updated_at
        "#,
        id,
        body.pattern,
        body.require_pr,
        body.block_force_push,
        body.required_approvals,
        body.dismiss_stale_reviews,
        &body.required_checks,
        body.require_up_to_date,
        body.allow_admin_bypass,
        &body.merge_methods,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "branch_protection.create",
            resource: "branch_protection",
            resource_id: Some(row.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"pattern": body.pattern})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(ProtectionResponse {
            id: row.id,
            project_id: row.project_id,
            pattern: row.pattern,
            require_pr: row.require_pr,
            block_force_push: row.block_force_push,
            required_approvals: row.required_approvals,
            dismiss_stale_reviews: row.dismiss_stale_reviews,
            required_checks: row.required_checks,
            require_up_to_date: row.require_up_to_date,
            allow_admin_bypass: row.allow_admin_bypass,
            merge_methods: row.merge_methods,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }),
    ))
}

async fn get_protection(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, rule_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ProtectionResponse>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let row = sqlx::query!(
        r#"
        SELECT id, project_id, pattern, require_pr, block_force_push,
               required_approvals, dismiss_stale_reviews, required_checks,
               require_up_to_date, allow_admin_bypass, merge_methods,
               created_at, updated_at
        FROM branch_protection_rules
        WHERE id = $1 AND project_id = $2
        "#,
        rule_id,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("branch protection rule".into()))?;

    Ok(Json(ProtectionResponse {
        id: row.id,
        project_id: row.project_id,
        pattern: row.pattern,
        require_pr: row.require_pr,
        block_force_push: row.block_force_push,
        required_approvals: row.required_approvals,
        dismiss_stale_reviews: row.dismiss_stale_reviews,
        required_checks: row.required_checks,
        require_up_to_date: row.require_up_to_date,
        allow_admin_bypass: row.allow_admin_bypass,
        merge_methods: row.merge_methods,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %rule_id), err)]
async fn update_protection(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, rule_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateProtectionRequest>,
) -> Result<Json<ProtectionResponse>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    if let Some(ref methods) = body.merge_methods {
        validate_merge_methods(methods)?;
    }
    if let Some(approvals) = body.required_approvals
        && approvals < 0
    {
        return Err(ApiError::BadRequest(
            "required_approvals must be >= 0".into(),
        ));
    }

    let row = sqlx::query!(
        r#"
        UPDATE branch_protection_rules SET
            require_pr = COALESCE($3, require_pr),
            block_force_push = COALESCE($4, block_force_push),
            required_approvals = COALESCE($5, required_approvals),
            dismiss_stale_reviews = COALESCE($6, dismiss_stale_reviews),
            required_checks = COALESCE($7, required_checks),
            require_up_to_date = COALESCE($8, require_up_to_date),
            allow_admin_bypass = COALESCE($9, allow_admin_bypass),
            merge_methods = COALESCE($10, merge_methods),
            updated_at = now()
        WHERE id = $1 AND project_id = $2
        RETURNING id, project_id, pattern, require_pr, block_force_push,
                  required_approvals, dismiss_stale_reviews, required_checks,
                  require_up_to_date, allow_admin_bypass, merge_methods,
                  created_at, updated_at
        "#,
        rule_id,
        id,
        body.require_pr,
        body.block_force_push,
        body.required_approvals,
        body.dismiss_stale_reviews,
        body.required_checks.as_deref(),
        body.require_up_to_date,
        body.allow_admin_bypass,
        body.merge_methods.as_deref(),
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("branch protection rule".into()))?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "branch_protection.update",
            resource: "branch_protection",
            resource_id: Some(rule_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(ProtectionResponse {
        id: row.id,
        project_id: row.project_id,
        pattern: row.pattern,
        require_pr: row.require_pr,
        block_force_push: row.block_force_push,
        required_approvals: row.required_approvals,
        dismiss_stale_reviews: row.dismiss_stale_reviews,
        required_checks: row.required_checks,
        require_up_to_date: row.require_up_to_date,
        allow_admin_bypass: row.allow_admin_bypass,
        merge_methods: row.merge_methods,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

#[tracing::instrument(skip(state), fields(%id, %rule_id), err)]
async fn delete_protection(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, rule_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let result = sqlx::query!(
        "DELETE FROM branch_protection_rules WHERE id = $1 AND project_id = $2",
        rule_id,
        id,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("branch protection rule".into()));
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "branch_protection.delete",
            resource: "branch_protection",
            resource_id: Some(rule_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_protection_inputs(
    pattern: &str,
    merge_methods: &[String],
    required_approvals: i32,
) -> Result<(), ApiError> {
    validation::check_length("pattern", pattern, 1, 255)?;
    if required_approvals < 0 {
        return Err(ApiError::BadRequest(
            "required_approvals must be >= 0".into(),
        ));
    }
    validate_merge_methods(merge_methods)
}

fn validate_merge_methods(methods: &[String]) -> Result<(), ApiError> {
    let valid = ["merge", "squash", "rebase"];
    for m in methods {
        if !valid.contains(&m.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "invalid merge method '{m}'; must be one of: merge, squash, rebase"
            )));
        }
    }
    if methods.is_empty() {
        return Err(ApiError::BadRequest(
            "merge_methods must contain at least one method".into(),
        ));
    }
    Ok(())
}
