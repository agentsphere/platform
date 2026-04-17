// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::helpers::require_project_read;
use crate::state::PlatformState;
use platform_auth::resolver;
use platform_types::{ApiError, AuditEntry, AuthUser, Permission, send_audit, validation};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        // Project-scoped flag CRUD
        .route(
            "/api/projects/{project_id}/flags",
            post(create_flag).get(list_flags),
        )
        .route(
            "/api/projects/{project_id}/flags/{key}",
            get(get_flag).patch(update_flag).delete(delete_flag),
        )
        .route(
            "/api/projects/{project_id}/flags/{key}/toggle",
            post(toggle_flag),
        )
        // Targeting rules
        .route(
            "/api/projects/{project_id}/flags/{key}/rules",
            post(add_rule),
        )
        .route(
            "/api/projects/{project_id}/flags/{key}/rules/{rule_id}",
            delete(delete_rule),
        )
        // User overrides
        .route(
            "/api/projects/{project_id}/flags/{key}/overrides/{user_id}",
            axum::routing::put(set_override).delete(delete_override),
        )
        // Audit trail
        .route(
            "/api/projects/{project_id}/flags/{key}/history",
            get(flag_history),
        )
        // Evaluation endpoint (for app SDKs)
        .route("/api/flags/evaluate", post(evaluate_flags))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

#[derive(Debug, Serialize)]
pub struct FlagResponse {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub key: String,
    pub flag_type: String,
    pub default_value: serde_json::Value,
    pub environment: Option<String>,
    pub enabled: bool,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateFlagRequest {
    pub key: String,
    #[serde(default = "default_flag_type")]
    pub flag_type: String,
    pub default_value: serde_json::Value,
    pub environment: Option<String>,
    pub description: Option<String>,
}

fn default_flag_type() -> String {
    "boolean".into()
}

#[derive(Debug, Deserialize)]
pub struct UpdateFlagRequest {
    pub default_value: Option<serde_json::Value>,
    pub description: Option<String>,
    pub flag_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RuleResponse {
    pub id: Uuid,
    pub flag_id: Uuid,
    pub priority: i32,
    pub rule_type: String,
    pub attribute_name: Option<String>,
    pub attribute_values: Vec<String>,
    pub percentage: Option<i32>,
    pub serve_value: serde_json::Value,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRuleRequest {
    pub priority: Option<i32>,
    pub rule_type: String,
    pub attribute_name: Option<String>,
    #[serde(default)]
    pub attribute_values: Vec<String>,
    pub percentage: Option<i32>,
    pub serve_value: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct SetOverrideRequest {
    pub serve_value: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct FlagHistoryResponse {
    pub id: Uuid,
    pub flag_id: Uuid,
    pub action: String,
    pub actor_id: Option<Uuid>,
    pub previous_value: Option<serde_json::Value>,
    pub new_value: Option<serde_json::Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Batch evaluation request from app SDKs.
#[derive(Debug, Deserialize)]
pub struct EvaluateRequest {
    pub project_id: Uuid,
    pub keys: Vec<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub attributes: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct EvaluateResponse {
    pub values: std::collections::HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Permission helper
// ---------------------------------------------------------------------------

async fn require_flag_manage(
    state: &PlatformState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::FlagManage,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;
    if !allowed {
        return Err(ApiError::NotFound("project".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn create_flag(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateFlagRequest>,
) -> Result<Json<FlagResponse>, ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    // Validate key
    validation::check_name(&body.key)?;
    if let Some(ref env) = body.environment
        && !["staging", "production"].contains(&env.as_str())
    {
        return Err(ApiError::BadRequest(
            "environment must be 'staging' or 'production'".into(),
        ));
    }
    let valid_types = ["boolean", "percentage", "variant", "json"];
    if !valid_types.contains(&body.flag_type.as_str()) {
        return Err(ApiError::BadRequest(
            "flag_type must be boolean, percentage, variant, or json".into(),
        ));
    }

    let row = sqlx::query(
        "INSERT INTO feature_flags (project_id, key, flag_type, default_value, environment, description, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, project_id, key, flag_type, default_value, environment, enabled, description, created_at, updated_at",
    )
    .bind(project_id)
    .bind(&body.key)
    .bind(&body.flag_type)
    .bind(&body.default_value)
    .bind(&body.environment)
    .bind(&body.description)
    .bind(auth.user_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            ApiError::Conflict("flag with this key already exists".into())
        }
        _ => ApiError::from(e),
    })?;

    let fid = flag_id(&row);

    record_flag_history(
        &state.pool,
        fid,
        "created",
        Some(auth.user_id),
        None,
        Some(&body.default_value),
    )
    .await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "flag.create".into(),
            resource: "feature_flag".into(),
            resource_id: Some(fid),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"key": body.key})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(row_to_flag(&row)))
}

async fn list_flags(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(project_id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<FlagResponse>>, ApiError> {
    require_project_read(&state, &auth, project_id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let rows = sqlx::query(
        "SELECT id, project_id, key, flag_type, default_value, environment, enabled, description, created_at, updated_at
         FROM feature_flags WHERE project_id = $1
         ORDER BY key ASC LIMIT $2 OFFSET $3",
    )
    .bind(project_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feature_flags WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&state.pool)
        .await?;

    let items = rows.iter().map(row_to_flag).collect();
    Ok(Json(ListResponse { items, total }))
}

async fn get_flag(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key)): Path<(Uuid, String)>,
) -> Result<Json<FlagResponse>, ApiError> {
    require_project_read(&state, &auth, project_id).await?;

    let row = sqlx::query(
        "SELECT id, project_id, key, flag_type, default_value, environment, enabled, description, created_at, updated_at
         FROM feature_flags WHERE project_id = $1 AND key = $2",
    )
    .bind(project_id)
    .bind(&key)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("flag".into()))?;

    Ok(Json(row_to_flag(&row)))
}

async fn update_flag(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key)): Path<(Uuid, String)>,
    Json(body): Json<UpdateFlagRequest>,
) -> Result<Json<FlagResponse>, ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    // Get current for audit
    let current = sqlx::query(
        "SELECT id, default_value FROM feature_flags WHERE project_id = $1 AND key = $2",
    )
    .bind(project_id)
    .bind(&key)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("flag".into()))?;

    let fid: Uuid = current.get("id");
    let prev_value: serde_json::Value = current.get("default_value");

    let row = sqlx::query(
        "UPDATE feature_flags SET
            default_value = COALESCE($3, default_value),
            description = COALESCE($4, description),
            flag_type = COALESCE($5, flag_type)
         WHERE project_id = $1 AND key = $2
         RETURNING id, project_id, key, flag_type, default_value, environment, enabled, description, created_at, updated_at",
    )
    .bind(project_id)
    .bind(&key)
    .bind(&body.default_value)
    .bind(&body.description)
    .bind(&body.flag_type)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("flag".into()))?;

    let new_value: serde_json::Value = row.get("default_value");
    record_flag_history(
        &state.pool,
        fid,
        "updated",
        Some(auth.user_id),
        Some(&prev_value),
        Some(&new_value),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "flag.update".into(),
            resource: "feature_flag".into(),
            resource_id: Some(fid),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"key": key})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    invalidate_flag_cache(&state.valkey, project_id, &key).await;

    Ok(Json(row_to_flag(&row)))
}

async fn delete_flag(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key)): Path<(Uuid, String)>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    let result =
        sqlx::query("DELETE FROM feature_flags WHERE project_id = $1 AND key = $2 RETURNING id")
            .bind(project_id)
            .bind(&key)
            .fetch_optional(&state.pool)
            .await?;

    if let Some(row) = result {
        let fid: Uuid = row.get("id");
        record_flag_history(&state.pool, fid, "deleted", Some(auth.user_id), None, None).await;
        send_audit(
            &state.audit_tx,
            AuditEntry {
                actor_id: auth.user_id,
                actor_name: auth.user_name.clone(),
                action: "flag.delete".into(),
                resource: "feature_flag".into(),
                resource_id: Some(fid),
                project_id: Some(project_id),
                detail: Some(serde_json::json!({"key": key})),
                ip_addr: auth.ip_addr.clone(),
            },
        );
        invalidate_flag_cache(&state.valkey, project_id, &key).await;
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("flag".into()))
    }
}

async fn toggle_flag(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key)): Path<(Uuid, String)>,
) -> Result<Json<FlagResponse>, ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    let row = sqlx::query(
        "UPDATE feature_flags SET enabled = NOT enabled
         WHERE project_id = $1 AND key = $2
         RETURNING id, project_id, key, flag_type, default_value, environment, enabled, description, created_at, updated_at",
    )
    .bind(project_id)
    .bind(&key)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("flag".into()))?;

    let fid: Uuid = row.get("id");
    let enabled: bool = row.get("enabled");
    record_flag_history(
        &state.pool,
        fid,
        "toggled",
        Some(auth.user_id),
        None,
        Some(&serde_json::json!({ "enabled": enabled })),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "flag.toggle".into(),
            resource: "feature_flag".into(),
            resource_id: Some(fid),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"key": key, "enabled": enabled})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    invalidate_flag_cache(&state.valkey, project_id, &key).await;

    Ok(Json(row_to_flag(&row)))
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

async fn add_rule(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key)): Path<(Uuid, String)>,
    Json(body): Json<CreateRuleRequest>,
) -> Result<(axum::http::StatusCode, Json<RuleResponse>), ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    let valid_types = ["user_id", "user_attribute", "percentage"];
    if !valid_types.contains(&body.rule_type.as_str()) {
        return Err(ApiError::BadRequest(
            "rule_type must be user_id, user_attribute, or percentage".into(),
        ));
    }
    if body.rule_type == "percentage" {
        match body.percentage {
            Some(p) if (0..=100).contains(&p) => {}
            _ => {
                return Err(ApiError::BadRequest(
                    "percentage rule requires percentage 0-100".into(),
                ));
            }
        }
    }

    let fid = get_flag_id(&state.pool, project_id, &key).await?;
    let priority = body.priority.unwrap_or(0);

    let row = sqlx::query(
        "INSERT INTO feature_flag_rules (flag_id, priority, rule_type, attribute_name, attribute_values, percentage, serve_value)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, flag_id, priority, rule_type, attribute_name, attribute_values, percentage, serve_value, enabled, created_at",
    )
    .bind(fid)
    .bind(priority)
    .bind(&body.rule_type)
    .bind(&body.attribute_name)
    .bind(&body.attribute_values)
    .bind(body.percentage)
    .bind(&body.serve_value)
    .fetch_one(&state.pool)
    .await?;

    record_flag_history(
        &state.pool,
        fid,
        "rule_added",
        Some(auth.user_id),
        None,
        Some(&body.serve_value),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "flag.rule.add".into(),
            resource: "feature_flag".into(),
            resource_id: Some(fid),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"key": key, "rule_type": body.rule_type})),
            ip_addr: auth.ip_addr.clone(),
        },
    );
    invalidate_flag_cache(&state.valkey, project_id, &key).await;

    Ok((axum::http::StatusCode::CREATED, Json(row_to_rule(&row))))
}

async fn delete_rule(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key, rule_id)): Path<(Uuid, String, Uuid)>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    let fid = get_flag_id(&state.pool, project_id, &key).await?;

    let result = sqlx::query("DELETE FROM feature_flag_rules WHERE id = $1 AND flag_id = $2")
        .bind(rule_id)
        .bind(fid)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("rule".into()));
    }

    record_flag_history(
        &state.pool,
        fid,
        "rule_deleted",
        Some(auth.user_id),
        None,
        None,
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "flag.rule.delete".into(),
            resource: "feature_flag".into(),
            resource_id: Some(fid),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"key": key, "rule_id": rule_id})),
            ip_addr: auth.ip_addr.clone(),
        },
    );
    invalidate_flag_cache(&state.valkey, project_id, &key).await;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Overrides
// ---------------------------------------------------------------------------

async fn set_override(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key, target_user_id)): Path<(Uuid, String, Uuid)>,
    Json(body): Json<SetOverrideRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    let fid = get_flag_id(&state.pool, project_id, &key).await?;

    sqlx::query(
        "INSERT INTO feature_flag_overrides (flag_id, user_id, serve_value)
         VALUES ($1, $2, $3)
         ON CONFLICT (flag_id, user_id) DO UPDATE SET serve_value = $3",
    )
    .bind(fid)
    .bind(target_user_id)
    .bind(&body.serve_value)
    .execute(&state.pool)
    .await?;

    record_flag_history(
        &state.pool,
        fid,
        "override_set",
        Some(auth.user_id),
        None,
        Some(&body.serve_value),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "flag.override.set".into(),
            resource: "feature_flag".into(),
            resource_id: Some(fid),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"key": key, "target_user_id": target_user_id})),
            ip_addr: auth.ip_addr.clone(),
        },
    );
    invalidate_flag_cache(&state.valkey, project_id, &key).await;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

async fn delete_override(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key, target_user_id)): Path<(Uuid, String, Uuid)>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_flag_manage(&state, &auth, project_id).await?;

    let fid = get_flag_id(&state.pool, project_id, &key).await?;

    let result =
        sqlx::query("DELETE FROM feature_flag_overrides WHERE flag_id = $1 AND user_id = $2")
            .bind(fid)
            .bind(target_user_id)
            .execute(&state.pool)
            .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("override".into()));
    }

    record_flag_history(
        &state.pool,
        fid,
        "override_deleted",
        Some(auth.user_id),
        None,
        None,
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "flag.override.delete".into(),
            resource: "feature_flag".into(),
            resource_id: Some(fid),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"key": key, "target_user_id": target_user_id})),
            ip_addr: auth.ip_addr.clone(),
        },
    );
    invalidate_flag_cache(&state.valkey, project_id, &key).await;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

async fn flag_history(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((project_id, key)): Path<(Uuid, String)>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<FlagHistoryResponse>>, ApiError> {
    require_project_read(&state, &auth, project_id).await?;

    let fid = get_flag_id(&state.pool, project_id, &key).await?;
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let rows = sqlx::query(
        "SELECT id, flag_id, action, actor_id, previous_value, new_value, created_at
         FROM feature_flag_history WHERE flag_id = $1
         ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(fid)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feature_flag_history WHERE flag_id = $1")
            .bind(fid)
            .fetch_one(&state.pool)
            .await?;

    let items = rows
        .iter()
        .map(|r| FlagHistoryResponse {
            id: r.get("id"),
            flag_id: r.get("flag_id"),
            action: r.get("action"),
            actor_id: r.get("actor_id"),
            previous_value: r.get("previous_value"),
            new_value: r.get("new_value"),
            created_at: r.get("created_at"),
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

// ---------------------------------------------------------------------------
// Evaluation (for app SDKs)
// ---------------------------------------------------------------------------

async fn evaluate_flags(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<EvaluateRequest>,
) -> Result<Json<EvaluateResponse>, ApiError> {
    // Require at least project read access for evaluation
    require_project_read(&state, &auth, body.project_id).await?;

    if body.keys.len() > 100 {
        return Err(ApiError::BadRequest(
            "too many evaluation keys (max 100)".into(),
        ));
    }

    let mut values = std::collections::HashMap::new();
    for key in &body.keys {
        let value = evaluate_single(
            &state.pool,
            &state.valkey,
            body.project_id,
            key,
            body.user_id.as_deref(),
            body.attributes.as_ref(),
        )
        .await;
        values.insert(key.clone(), value);
    }

    Ok(Json(EvaluateResponse { values }))
}

/// Evaluate a single flag for a given context.
///
/// Evaluation order:
/// 1. Flag disabled → `default_value`
/// 2. Per-user override → override value
/// 3. Targeting rules by priority (`user_id`, `user_attribute`, percentage)
/// 4. No match → `default_value`
pub async fn evaluate_single(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    project_id: Uuid,
    key: &str,
    user_id: Option<&str>,
    attributes: Option<&serde_json::Value>,
) -> serde_json::Value {
    // Try cache first
    let cache_key = format!(
        "flag:{project_id}:{key}:{}",
        user_id.unwrap_or("_anonymous")
    );
    if let Ok(Some(ref s)) =
        fred::interfaces::KeysInterface::get::<Option<String>, _>(valkey, &cache_key).await
        && let Ok(v) = serde_json::from_str(s)
    {
        return v;
    }

    let result = evaluate_single_uncached(pool, project_id, key, user_id, attributes).await;

    // Cache result (60s TTL as fallback)
    let _ = fred::interfaces::KeysInterface::set::<(), _, _>(
        valkey,
        &cache_key,
        serde_json::to_string(&result).unwrap_or_default(),
        Some(fred::types::Expiration::EX(10)),
        None,
        false,
    )
    .await;

    result
}

async fn evaluate_single_uncached(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    key: &str,
    user_id: Option<&str>,
    attributes: Option<&serde_json::Value>,
) -> serde_json::Value {
    // Load flag
    let flag = sqlx::query(
        "SELECT id, enabled, default_value FROM feature_flags
         WHERE project_id = $1 AND key = $2",
    )
    .bind(project_id)
    .bind(key)
    .fetch_optional(pool)
    .await;

    let Ok(Some(flag)) = flag else {
        return serde_json::Value::Null;
    };

    let flag_id: Uuid = flag.get("id");
    let enabled: bool = flag.get("enabled");
    let default_value: serde_json::Value = flag.get("default_value");

    // 1. Disabled → default
    if !enabled {
        return default_value;
    }

    // 2. Per-user override
    if let Some(uid_str) = user_id
        && let Ok(uid) = uid_str.parse::<Uuid>()
        && let Ok(Some(row)) = sqlx::query(
            "SELECT serve_value FROM feature_flag_overrides
             WHERE flag_id = $1 AND user_id = $2",
        )
        .bind(flag_id)
        .bind(uid)
        .fetch_optional(pool)
        .await
    {
        return row.get("serve_value");
    }

    // 3. Targeting rules by priority
    let rules = sqlx::query(
        "SELECT rule_type, attribute_name, attribute_values, percentage, serve_value
         FROM feature_flag_rules
         WHERE flag_id = $1 AND enabled = true
         ORDER BY priority DESC",
    )
    .bind(flag_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for rule in &rules {
        let rule_type: String = rule.get("rule_type");
        let serve_value: serde_json::Value = rule.get("serve_value");

        match rule_type.as_str() {
            "user_id" => {
                let attr_values: Vec<String> = rule.get("attribute_values");
                if let Some(uid) = user_id
                    && attr_values.iter().any(|v| v == uid)
                {
                    return serve_value;
                }
            }
            "user_attribute" => {
                if let Some(attrs) = attributes {
                    let attr_name: Option<String> = rule.get("attribute_name");
                    let attr_values: Vec<String> = rule.get("attribute_values");
                    if let Some(ref name) = attr_name
                        && let Some(user_val) = attrs.get(name).and_then(|v| v.as_str())
                        && attr_values.iter().any(|v| v == user_val)
                    {
                        return serve_value;
                    }
                }
            }
            "percentage" => {
                let pct: Option<i32> = rule.get("percentage");
                if let (Some(pct), Some(uid)) = (pct, user_id) {
                    let hash = fnv1a_hash(&format!("{key}:{uid}"));
                    #[allow(clippy::cast_sign_loss)]
                    if (hash % 100) < pct as u64 {
                        return serve_value;
                    }
                }
            }
            _ => {}
        }
    }

    // 4. Default
    default_value
}

/// FNV-1a hash for deterministic percentage bucketing.
fn fnv1a_hash(input: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in input.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use sqlx::Row;

fn flag_id(row: &sqlx::postgres::PgRow) -> Uuid {
    row.get("id")
}

fn row_to_flag(row: &sqlx::postgres::PgRow) -> FlagResponse {
    FlagResponse {
        id: row.get("id"),
        project_id: row.get("project_id"),
        key: row.get("key"),
        flag_type: row.get("flag_type"),
        default_value: row.get("default_value"),
        environment: row.get("environment"),
        enabled: row.get("enabled"),
        description: row.get("description"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_rule(row: &sqlx::postgres::PgRow) -> RuleResponse {
    RuleResponse {
        id: row.get("id"),
        flag_id: row.get("flag_id"),
        priority: row.get("priority"),
        rule_type: row.get("rule_type"),
        attribute_name: row.get("attribute_name"),
        attribute_values: row.get("attribute_values"),
        percentage: row.get("percentage"),
        serve_value: row.get("serve_value"),
        enabled: row.get("enabled"),
        created_at: row.get("created_at"),
    }
}

async fn get_flag_id(pool: &sqlx::PgPool, project_id: Uuid, key: &str) -> Result<Uuid, ApiError> {
    sqlx::query_scalar("SELECT id FROM feature_flags WHERE project_id = $1 AND key = $2")
        .bind(project_id)
        .bind(key)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("flag".into()))
}

async fn record_flag_history(
    pool: &sqlx::PgPool,
    flag_id: Uuid,
    action: &str,
    actor_id: Option<Uuid>,
    previous_value: Option<&serde_json::Value>,
    new_value: Option<&serde_json::Value>,
) {
    let _ = sqlx::query(
        "INSERT INTO feature_flag_history (flag_id, action, actor_id, previous_value, new_value)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(flag_id)
    .bind(action)
    .bind(actor_id)
    .bind(previous_value)
    .bind(new_value)
    .execute(pool)
    .await;
}

async fn invalidate_flag_cache(valkey: &fred::clients::Pool, project_id: Uuid, key: &str) {
    // Invalidate the anonymous cache key.
    // Per-user keys expire naturally (60s TTL). For toggling during an incident,
    // the anonymous key is the critical one. Users re-evaluate on next miss.
    let cache_key = format!("flag:{project_id}:{key}:_anonymous");
    let _ = fred::interfaces::KeysInterface::del::<(), _>(valkey, &cache_key).await;
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_deterministic() {
        let h1 = fnv1a_hash("my_flag:user123");
        let h2 = fnv1a_hash("my_flag:user123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn fnv1a_different_inputs_different_hashes() {
        let h1 = fnv1a_hash("flag_a:user1");
        let h2 = fnv1a_hash("flag_b:user1");
        assert_ne!(h1, h2);
    }

    #[test]
    fn fnv1a_percentage_distribution() {
        // Verify rough uniformity over many inputs
        let mut under_50 = 0u64;
        let total = 10000u64;
        for i in 0..total {
            let hash = fnv1a_hash(&format!("test_flag:user_{i}"));
            if hash % 100 < 50 {
                under_50 += 1;
            }
        }
        // Should be roughly 50% ± 5%
        #[allow(clippy::cast_precision_loss)]
        let pct = (under_50 as f64 / total as f64) * 100.0;
        assert!(
            (45.0..=55.0).contains(&pct),
            "distribution should be roughly uniform, got {pct:.1}%"
        );
    }

    #[test]
    fn fnv1a_empty_string() {
        let h = fnv1a_hash("");
        assert_ne!(h, 0);
    }
}
