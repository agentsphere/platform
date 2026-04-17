// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Alert rule CRUD and alert event query endpoints.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fred::interfaces::PubsubInterface;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use platform_auth::resolver;
use platform_types::{ApiError, AuditEntry, AuthUser, Permission, send_audit, validation};

use super::helpers::ListResponse;
use crate::state::PlatformState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateAlertRequest {
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    pub condition: String,
    pub threshold: Option<f64>,
    #[serde(alias = "window_seconds")]
    pub for_seconds: Option<i32>,
    pub severity: Option<String>,
    #[serde(alias = "channels")]
    pub notify_channels: Option<Vec<String>>,
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAlertRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub query: Option<String>,
    pub condition: Option<String>,
    pub threshold: Option<f64>,
    #[serde(alias = "window_seconds")]
    pub for_seconds: Option<i32>,
    pub severity: Option<String>,
    #[serde(alias = "channels")]
    pub notify_channels: Option<Vec<String>>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListAlertParams {
    pub project_id: Option<Uuid>,
    pub enabled: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AlertRuleResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    pub condition: String,
    pub threshold: Option<f64>,
    #[serde(rename = "window_seconds")]
    pub for_seconds: i32,
    pub severity: String,
    #[serde(rename = "channels")]
    pub notify_channels: Vec<String>,
    pub project_id: Option<Uuid>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct AlertEventResponse {
    pub id: Uuid,
    #[serde(rename = "alert_rule_id")]
    pub rule_id: Uuid,
    pub status: String,
    pub value: Option<f64>,
    pub message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Alert query DSL
// ---------------------------------------------------------------------------

/// Parsed alert query. Format: `metric:<name> [labels:{json}] [agg:<func>] [window:<secs>]`
struct AlertQuery {
    #[allow(dead_code)]
    metric_name: String,
    #[allow(dead_code)]
    labels: Option<serde_json::Value>,
    #[allow(dead_code)]
    aggregation: String,
    #[allow(dead_code)]
    window_secs: i32,
}

fn parse_alert_query(query: &str) -> Result<AlertQuery, ApiError> {
    validation::check_length("query", query, 1, 1000)?;

    let mut metric_name = None;
    let mut labels = None;
    let mut aggregation = "avg".to_string();
    let mut window_secs: i32 = 300;

    for part in query.split_whitespace() {
        if let Some(name) = part.strip_prefix("metric:") {
            validation::check_length("metric_name", name, 1, 255)?;
            metric_name = Some(name.to_string());
        } else if let Some(json) = part.strip_prefix("labels:") {
            labels = Some(
                serde_json::from_str(json)
                    .map_err(|_| ApiError::BadRequest("invalid labels JSON in query".into()))?,
            );
        } else if let Some(agg) = part.strip_prefix("agg:") {
            if !["avg", "sum", "max", "min", "count"].contains(&agg) {
                return Err(ApiError::BadRequest(format!("unknown aggregation: {agg}")));
            }
            aggregation = agg.to_string();
        } else if let Some(w) = part.strip_prefix("window:") {
            window_secs = w
                .parse()
                .map_err(|_| ApiError::BadRequest("window must be an integer (seconds)".into()))?;
            if !(10..=86400).contains(&window_secs) {
                return Err(ApiError::BadRequest(
                    "window must be between 10 and 86400 seconds".into(),
                ));
            }
        }
    }

    let metric_name = metric_name
        .ok_or_else(|| ApiError::BadRequest("query must include metric:<name>".into()))?;

    Ok(AlertQuery {
        metric_name,
        labels,
        aggregation,
        window_secs,
    })
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route("/api/observe/alerts", get(list_alerts).post(create_alert))
        .route("/api/observe/alerts/events", get(list_all_alert_events))
        .route(
            "/api/observe/alerts/{id}",
            get(get_alert).patch(update_alert).delete(delete_alert),
        )
        .route("/api/observe/alerts/{id}/events", get(list_alert_events))
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

async fn require_alert_manage(state: &PlatformState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AlertManage,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_observe_read(state: &PlatformState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ObserveRead,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_condition(condition: &str) -> Result<(), ApiError> {
    if !["gt", "lt", "eq", "absent"].contains(&condition) {
        return Err(ApiError::BadRequest(
            "condition must be gt, lt, eq, or absent".into(),
        ));
    }
    Ok(())
}

/// Notify the observe crate's `alert_rule_subscriber` to rebuild the `AlertRouter`.
async fn notify_alert_rules_changed(state: &PlatformState) {
    let result: Result<(), _> = state
        .valkey
        .next()
        .publish(
            platform_observe::alert::ALERT_RULES_CHANGED_CHANNEL,
            "changed",
        )
        .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to publish alert rules changed");
    }
}

// ---------------------------------------------------------------------------
// Row → response mapping
// ---------------------------------------------------------------------------

fn row_to_rule(r: &sqlx::postgres::PgRow) -> AlertRuleResponse {
    AlertRuleResponse {
        id: r.get("id"),
        name: r.get("name"),
        description: r.get("description"),
        query: r.get("query"),
        condition: r.get("condition"),
        threshold: r.get("threshold"),
        for_seconds: r.get("for_seconds"),
        severity: r.get("severity"),
        notify_channels: r.get("notify_channels"),
        project_id: r.get("project_id"),
        enabled: r.get("enabled"),
        created_at: r.get("created_at"),
    }
}

fn row_to_event(r: &sqlx::postgres::PgRow) -> AlertEventResponse {
    AlertEventResponse {
        id: r.get("id"),
        rule_id: r.get("rule_id"),
        status: r.get("status"),
        value: r.get("value"),
        message: r.get("message"),
        created_at: r.get("created_at"),
        resolved_at: r.get("resolved_at"),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state), err)]
async fn list_alerts(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Query(params): Query<ListAlertParams>,
) -> Result<Json<ListResponse<AlertRuleResponse>>, ApiError> {
    require_observe_read(&state, &auth).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM alert_rules
        WHERE ($1::uuid IS NULL OR project_id = $1)
          AND ($2::bool IS NULL OR enabled = $2)
        ",
    )
    .bind(params.project_id)
    .bind(params.enabled)
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query(
        r"
        SELECT id, name, description, query, condition, threshold,
               for_seconds, severity, notify_channels, project_id,
               enabled, created_at
        FROM alert_rules
        WHERE ($1::uuid IS NULL OR project_id = $1)
          AND ($2::bool IS NULL OR enabled = $2)
        ORDER BY created_at DESC
        LIMIT $3 OFFSET $4
        ",
    )
    .bind(params.project_id)
    .bind(params.enabled)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows.iter().map(row_to_rule).collect();
    Ok(Json(ListResponse { items, total }))
}

#[tracing::instrument(skip(state, body), fields(alert_name = %body.name), err)]
async fn create_alert(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<CreateAlertRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_alert_manage(&state, &auth).await?;

    validation::check_length("name", &body.name, 1, 255)?;
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }

    // Validate query DSL
    parse_alert_query(&body.query)?;

    validate_condition(&body.condition)?;

    let for_seconds = body.for_seconds.unwrap_or(60);
    if !(10..=3600).contains(&for_seconds) {
        return Err(ApiError::BadRequest(
            "for_seconds must be between 10 and 3600".into(),
        ));
    }

    let severity = body.severity.as_deref().unwrap_or("warning");
    if !["info", "warning", "critical"].contains(&severity) {
        return Err(ApiError::BadRequest(
            "severity must be info, warning, or critical".into(),
        ));
    }

    let channels = body.notify_channels.as_deref().unwrap_or(&[]);

    let row = sqlx::query(
        r"
        INSERT INTO alert_rules (name, description, query, condition, threshold,
                                 for_seconds, severity, notify_channels, project_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, name, description, query, condition, threshold,
                  for_seconds, severity, notify_channels, project_id,
                  enabled, created_at
        ",
    )
    .bind(&body.name)
    .bind(&body.description)
    .bind(&body.query)
    .bind(&body.condition)
    .bind(body.threshold)
    .bind(for_seconds)
    .bind(severity)
    .bind(channels)
    .bind(body.project_id)
    .fetch_one(&state.pool)
    .await?;

    let resp = row_to_rule(&row);

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "alert.create".into(),
            resource: "alert".into(),
            resource_id: Some(resp.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    notify_alert_rules_changed(&state).await;

    Ok((StatusCode::CREATED, Json(resp)))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn get_alert(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<AlertRuleResponse>, ApiError> {
    require_observe_read(&state, &auth).await?;

    let row = sqlx::query(
        r"
        SELECT id, name, description, query, condition, threshold,
               for_seconds, severity, notify_channels, project_id,
               enabled, created_at
        FROM alert_rules WHERE id = $1
        ",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("alert rule".into()))?;

    Ok(Json(row_to_rule(&row)))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn update_alert(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateAlertRequest>,
) -> Result<Json<AlertRuleResponse>, ApiError> {
    require_alert_manage(&state, &auth).await?;

    if let Some(ref name) = body.name {
        validation::check_length("name", name, 1, 255)?;
    }
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }
    if let Some(ref query) = body.query {
        parse_alert_query(query)?;
    }
    if let Some(ref condition) = body.condition {
        validate_condition(condition)?;
    }
    if let Some(for_seconds) = body.for_seconds
        && !(10..=3600).contains(&for_seconds)
    {
        return Err(ApiError::BadRequest(
            "for_seconds must be between 10 and 3600".into(),
        ));
    }
    if let Some(ref severity) = body.severity
        && !["info", "warning", "critical"].contains(&severity.as_str())
    {
        return Err(ApiError::BadRequest(
            "severity must be info, warning, or critical".into(),
        ));
    }

    let row = sqlx::query(
        r"
        UPDATE alert_rules SET
            name = COALESCE($2, name),
            description = COALESCE($3, description),
            query = COALESCE($4, query),
            condition = COALESCE($5, condition),
            threshold = COALESCE($6, threshold),
            for_seconds = COALESCE($7, for_seconds),
            severity = COALESCE($8, severity),
            notify_channels = COALESCE($9, notify_channels),
            enabled = COALESCE($10, enabled)
        WHERE id = $1
        RETURNING id, name, description, query, condition, threshold,
                  for_seconds, severity, notify_channels, project_id,
                  enabled, created_at
        ",
    )
    .bind(id)
    .bind(&body.name)
    .bind(&body.description)
    .bind(&body.query)
    .bind(&body.condition)
    .bind(body.threshold)
    .bind(body.for_seconds)
    .bind(&body.severity)
    .bind(body.notify_channels.as_deref())
    .bind(body.enabled)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("alert rule".into()))?;

    let resp = row_to_rule(&row);

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "alert.update".into(),
            resource: "alert".into(),
            resource_id: Some(id),
            project_id: resp.project_id,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    notify_alert_rules_changed(&state).await;

    Ok(Json(resp))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn delete_alert(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_alert_manage(&state, &auth).await?;

    let result = sqlx::query("DELETE FROM alert_rules WHERE id = $1 RETURNING project_id")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("alert rule".into()))?;

    let deleted_project_id: Option<Uuid> = result.get("project_id");

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "alert.delete".into(),
            resource: "alert".into(),
            resource_id: Some(id),
            project_id: deleted_project_id,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    notify_alert_rules_changed(&state).await;

    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn list_alert_events(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListAlertParams>,
) -> Result<Json<ListResponse<AlertEventResponse>>, ApiError> {
    require_observe_read(&state, &auth).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alert_events WHERE rule_id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;

    let rows = sqlx::query(
        r"
        SELECT id, rule_id, status, value, message, created_at, resolved_at
        FROM alert_events
        WHERE rule_id = $1
        ORDER BY created_at DESC
        LIMIT $2 OFFSET $3
        ",
    )
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows.iter().map(row_to_event).collect();
    Ok(Json(ListResponse { items, total }))
}

#[tracing::instrument(skip(state), err)]
async fn list_all_alert_events(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Query(params): Query<ListAlertParams>,
) -> Result<Json<ListResponse<AlertEventResponse>>, ApiError> {
    require_observe_read(&state, &auth).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alert_events")
        .fetch_one(&state.pool)
        .await?;

    let rows = sqlx::query(
        r"
        SELECT id, rule_id, status, value, message, created_at, resolved_at
        FROM alert_events
        ORDER BY created_at DESC
        LIMIT $1 OFFSET $2
        ",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows.iter().map(row_to_event).collect();
    Ok(Json(ListResponse { items, total }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_alert_query_basic() {
        let q = parse_alert_query("metric:cpu_usage agg:avg window:300").unwrap();
        assert_eq!(q.metric_name, "cpu_usage");
        assert_eq!(q.aggregation, "avg");
        assert_eq!(q.window_secs, 300);
        assert!(q.labels.is_none());
    }

    #[test]
    fn parse_alert_query_with_labels() {
        let q = parse_alert_query(r#"metric:mem labels:{"env":"prod"} agg:sum window:60"#).unwrap();
        assert_eq!(q.metric_name, "mem");
        assert_eq!(q.aggregation, "sum");
        assert_eq!(q.window_secs, 60);
        assert!(q.labels.is_some());
    }

    #[test]
    fn parse_alert_query_defaults() {
        let q = parse_alert_query("metric:disk").unwrap();
        assert_eq!(q.aggregation, "avg");
        assert_eq!(q.window_secs, 300);
    }

    #[test]
    fn parse_alert_query_missing_metric() {
        assert!(parse_alert_query("agg:sum window:60").is_err());
    }

    #[test]
    fn parse_alert_query_bad_agg() {
        assert!(parse_alert_query("metric:x agg:median").is_err());
    }

    #[test]
    fn parse_alert_query_window_out_of_range() {
        assert!(parse_alert_query("metric:x window:5").is_err());
        assert!(parse_alert_query("metric:x window:100000").is_err());
    }

    #[test]
    fn validate_condition_valid() {
        for c in &["gt", "lt", "eq", "absent"] {
            assert!(validate_condition(c).is_ok());
        }
    }

    #[test]
    fn validate_condition_invalid() {
        assert!(validate_condition("gte").is_err());
        assert!(validate_condition("").is_err());
    }

    #[test]
    fn router_builds_without_panic() {
        let _r: Router<PlatformState> = router();
    }
}
