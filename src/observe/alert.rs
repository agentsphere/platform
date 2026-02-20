use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateAlertRequest {
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    pub condition: String,
    pub threshold: Option<f64>,
    pub for_seconds: Option<i32>,
    pub severity: Option<String>,
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
    pub for_seconds: Option<i32>,
    pub severity: Option<String>,
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
    pub for_seconds: i32,
    pub severity: String,
    pub notify_channels: Vec<String>,
    pub project_id: Option<Uuid>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct AlertEventResponse {
    pub id: Uuid,
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
    metric_name: String,
    labels: Option<serde_json::Value>,
    aggregation: String,
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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/observe/alerts", get(list_alerts).post(create_alert))
        .route(
            "/api/observe/alerts/{id}",
            get(get_alert).patch(update_alert).delete(delete_alert),
        )
        .route("/api/observe/alerts/{id}/events", get(list_alert_events))
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

async fn require_alert_manage(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AlertManage,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_observe_read(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ObserveRead,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state), err)]
async fn list_alerts(
    State(state): State<AppState>,
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

    let items = rows
        .into_iter()
        .map(|r| AlertRuleResponse {
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
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

#[tracing::instrument(skip(state, body), fields(alert_name = %body.name), err)]
async fn create_alert(
    State(state): State<AppState>,
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

    let row_id: Uuid = row.get("id");

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "alert.create",
            resource: "alert",
            resource_id: Some(row_id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(AlertRuleResponse {
            id: row_id,
            name: row.get("name"),
            description: row.get("description"),
            query: row.get("query"),
            condition: row.get("condition"),
            threshold: row.get("threshold"),
            for_seconds: row.get("for_seconds"),
            severity: row.get("severity"),
            notify_channels: row.get("notify_channels"),
            project_id: row.get("project_id"),
            enabled: row.get("enabled"),
            created_at: row.get("created_at"),
        }),
    ))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn get_alert(
    State(state): State<AppState>,
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

    Ok(Json(AlertRuleResponse {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        query: row.get("query"),
        condition: row.get("condition"),
        threshold: row.get("threshold"),
        for_seconds: row.get("for_seconds"),
        severity: row.get("severity"),
        notify_channels: row.get("notify_channels"),
        project_id: row.get("project_id"),
        enabled: row.get("enabled"),
        created_at: row.get("created_at"),
    }))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn update_alert(
    State(state): State<AppState>,
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

    let row_project_id: Option<Uuid> = row.get("project_id");

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "alert.update",
            resource: "alert",
            resource_id: Some(id),
            project_id: row_project_id,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(AlertRuleResponse {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        query: row.get("query"),
        condition: row.get("condition"),
        threshold: row.get("threshold"),
        for_seconds: row.get("for_seconds"),
        severity: row.get("severity"),
        notify_channels: row.get("notify_channels"),
        project_id: row_project_id,
        enabled: row.get("enabled"),
        created_at: row.get("created_at"),
    }))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn delete_alert(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_alert_manage(&state, &auth).await?;

    let result = sqlx::query("DELETE FROM alert_rules WHERE id = $1 RETURNING project_id")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("alert rule".into()))?;

    let deleted_project_id: Option<Uuid> = result.get("project_id");

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "alert.delete",
            resource: "alert",
            resource_id: Some(id),
            project_id: deleted_project_id,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn list_alert_events(
    State(state): State<AppState>,
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

    let items = rows
        .into_iter()
        .map(|r| AlertEventResponse {
            id: r.get("id"),
            rule_id: r.get("rule_id"),
            status: r.get("status"),
            value: r.get("value"),
            message: r.get("message"),
            created_at: r.get("created_at"),
            resolved_at: r.get("resolved_at"),
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
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

// ---------------------------------------------------------------------------
// Background evaluation
// ---------------------------------------------------------------------------

struct AlertState {
    first_triggered: Option<DateTime<Utc>>,
    firing: bool,
}

/// Background task that evaluates alert rules every 30 seconds.
pub async fn evaluate_alerts_loop(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("alert evaluator started");
    let mut alert_states: HashMap<Uuid, AlertState> = HashMap::new();

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("alert evaluator shutting down");
                break;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                if let Err(e) = evaluate_all(&state, &mut alert_states).await {
                    tracing::error!(error = %e, "alert evaluation cycle failed");
                }
            }
        }
    }
}

async fn evaluate_all(
    state: &AppState,
    alert_states: &mut HashMap<Uuid, AlertState>,
) -> Result<(), anyhow::Error> {
    let rules = sqlx::query(
        "SELECT id, query, condition, threshold, for_seconds FROM alert_rules WHERE enabled = true",
    )
    .fetch_all(&state.pool)
    .await?;

    for rule in &rules {
        let rule_id: Uuid = rule.get("id");
        let rule_query: String = rule.get("query");
        let rule_condition: String = rule.get("condition");
        let rule_threshold: Option<f64> = rule.get("threshold");
        let rule_for_seconds: i32 = rule.get("for_seconds");

        let aq = match parse_alert_query(&rule_query) {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(rule_id = %rule_id, error = %e, "invalid alert query");
                continue;
            }
        };

        let value = evaluate_metric(
            &state.pool,
            &aq.metric_name,
            aq.labels.as_ref(),
            &aq.aggregation,
            aq.window_secs,
        )
        .await;

        let value = match value {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(rule_id = %rule_id, error = %e, "metric evaluation failed");
                continue;
            }
        };

        let condition_met = check_condition(&rule_condition, rule_threshold, value);

        let now = Utc::now();
        let as_entry = alert_states.entry(rule_id).or_insert(AlertState {
            first_triggered: None,
            firing: false,
        });

        handle_alert_state(
            &state.pool,
            rule_id,
            rule_for_seconds,
            condition_met,
            value,
            now,
            as_entry,
        )
        .await;
    }

    Ok(())
}

async fn handle_alert_state(
    pool: &sqlx::PgPool,
    rule_id: Uuid,
    for_seconds: i32,
    condition_met: bool,
    value: Option<f64>,
    now: DateTime<Utc>,
    state: &mut AlertState,
) {
    if condition_met {
        if state.first_triggered.is_none() {
            state.first_triggered = Some(now);
        }
        let held_for = (now - state.first_triggered.unwrap()).num_seconds();
        if held_for >= i64::from(for_seconds) && !state.firing {
            state.firing = true;
            let _ = fire_alert(pool, rule_id, value).await;
        }
    } else {
        if state.firing {
            let _ = resolve_alert(pool, rule_id).await;
        }
        state.first_triggered = None;
        state.firing = false;
    }
}

fn check_condition(condition: &str, threshold: Option<f64>, value: Option<f64>) -> bool {
    match condition {
        "absent" => value.is_none(),
        "gt" => value.is_some_and(|v| threshold.is_some_and(|t| v > t)),
        "lt" => value.is_some_and(|v| threshold.is_some_and(|t| v < t)),
        "eq" => value.is_some_and(|v| threshold.is_some_and(|t| (v - t).abs() < f64::EPSILON)),
        _ => false,
    }
}

async fn evaluate_metric(
    pool: &sqlx::PgPool,
    name: &str,
    labels: Option<&serde_json::Value>,
    agg: &str,
    window_secs: i32,
) -> Result<Option<f64>, sqlx::Error> {
    let interval = format!("{window_secs} seconds");
    match agg {
        "avg" => {
            sqlx::query_scalar::<_, Option<f64>>(
                r"SELECT AVG(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3::interval",
            )
            .bind(name)
            .bind(labels)
            .bind(&interval)
            .fetch_one(pool)
            .await
        }
        "sum" => {
            sqlx::query_scalar::<_, Option<f64>>(
                r"SELECT SUM(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3::interval",
            )
            .bind(name)
            .bind(labels)
            .bind(&interval)
            .fetch_one(pool)
            .await
        }
        "max" => {
            sqlx::query_scalar::<_, Option<f64>>(
                r"SELECT MAX(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3::interval",
            )
            .bind(name)
            .bind(labels)
            .bind(&interval)
            .fetch_one(pool)
            .await
        }
        "min" => {
            sqlx::query_scalar::<_, Option<f64>>(
                r"SELECT MIN(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3::interval",
            )
            .bind(name)
            .bind(labels)
            .bind(&interval)
            .fetch_one(pool)
            .await
        }
        "count" => {
            let count: Option<i64> = sqlx::query_scalar(
                r"SELECT COUNT(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3::interval",
            )
            .bind(name)
            .bind(labels)
            .bind(&interval)
            .fetch_one(pool)
            .await?;
            #[allow(clippy::cast_precision_loss)]
            Ok(count.map(|c| c as f64))
        }
        _ => Ok(None),
    }
}

async fn fire_alert(
    pool: &sqlx::PgPool,
    rule_id: Uuid,
    value: Option<f64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r"
        INSERT INTO alert_events (rule_id, status, value, message)
        VALUES ($1, 'firing', $2, 'Alert condition met')
        ",
    )
    .bind(rule_id)
    .bind(value)
    .execute(pool)
    .await?;

    tracing::warn!(rule_id = %rule_id, ?value, "alert firing");
    Ok(())
}

async fn resolve_alert(pool: &sqlx::PgPool, rule_id: Uuid) -> Result<(), sqlx::Error> {
    // Resolve the most recent firing event for this rule
    sqlx::query(
        r"
        UPDATE alert_events SET status = 'resolved', resolved_at = now()
        WHERE rule_id = $1 AND status = 'firing' AND resolved_at IS NULL
        ",
    )
    .bind(rule_id)
    .execute(pool)
    .await?;

    tracing::info!(rule_id = %rule_id, "alert resolved");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_query() {
        let q = parse_alert_query("metric:cpu_usage agg:avg window:300").unwrap();
        assert_eq!(q.metric_name, "cpu_usage");
        assert_eq!(q.aggregation, "avg");
        assert_eq!(q.window_secs, 300);
        assert!(q.labels.is_none());
    }

    #[test]
    fn parse_query_with_labels() {
        let q = parse_alert_query(r#"metric:http_errors labels:{"method":"GET"} agg:sum"#).unwrap();
        assert_eq!(q.metric_name, "http_errors");
        assert_eq!(q.aggregation, "sum");
        assert!(q.labels.is_some());
    }

    #[test]
    fn parse_query_defaults() {
        let q = parse_alert_query("metric:mem_usage").unwrap();
        assert_eq!(q.aggregation, "avg");
        assert_eq!(q.window_secs, 300);
    }

    #[test]
    fn parse_query_missing_metric() {
        assert!(parse_alert_query("agg:sum").is_err());
    }

    #[test]
    fn parse_query_invalid_agg() {
        assert!(parse_alert_query("metric:foo agg:median").is_err());
    }

    #[test]
    fn condition_gt() {
        assert!(check_condition("gt", Some(10.0), Some(15.0)));
        assert!(!check_condition("gt", Some(10.0), Some(5.0)));
    }

    #[test]
    fn condition_lt() {
        assert!(check_condition("lt", Some(10.0), Some(5.0)));
        assert!(!check_condition("lt", Some(10.0), Some(15.0)));
    }

    #[test]
    fn condition_eq() {
        assert!(check_condition("eq", Some(10.0), Some(10.0)));
        assert!(!check_condition("eq", Some(10.0), Some(10.1)));
    }

    #[test]
    fn condition_absent() {
        assert!(check_condition("absent", None, None));
        assert!(!check_condition("absent", None, Some(5.0)));
    }
}
