// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Alert state machine, evaluation logic, and DB operations.
//!
//! HTTP CRUD handlers (list, create, get, update, delete) stay in the main
//! binary. This module provides the core evaluation engine callable from
//! any binary.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use platform_types::ApiError;

// ---------------------------------------------------------------------------
// Alert query DSL
// ---------------------------------------------------------------------------

/// Parsed alert query. Format: `metric:<name> [labels:{json}] [agg:<func>] [window:<secs>]`
pub struct AlertQuery {
    pub metric_name: String,
    pub labels: Option<serde_json::Value>,
    pub aggregation: String,
    pub window_secs: i32,
}

pub fn parse_alert_query(query: &str) -> Result<AlertQuery, ApiError> {
    platform_types::validation::check_length("query", query, 1, 1000)?;

    let mut metric_name = None;
    let mut labels = None;
    let mut aggregation = "avg".to_string();
    let mut window_secs: i32 = 300;

    for part in query.split_whitespace() {
        if let Some(name) = part.strip_prefix("metric:") {
            platform_types::validation::check_length("metric_name", name, 1, 255)?;
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
// Validation helpers
// ---------------------------------------------------------------------------

pub fn validate_condition(condition: &str) -> Result<(), ApiError> {
    if !["gt", "lt", "eq", "absent"].contains(&condition) {
        return Err(ApiError::BadRequest(
            "condition must be gt, lt, eq, or absent".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Alert state machine
// ---------------------------------------------------------------------------

/// In-memory state for an alert rule during evaluation.
pub struct AlertState {
    pub first_triggered: Option<DateTime<Utc>>,
    pub firing: bool,
}

/// Result of evaluating the alert state transition.
pub struct AlertTransition {
    /// Whether the alert should fire (transition to firing).
    pub should_fire: bool,
    /// Whether the alert should resolve (was firing, condition cleared).
    pub should_resolve: bool,
}

/// Metadata about an alert rule, passed to `handle_alert_state`.
pub struct AlertRuleInfo<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub severity: &'a str,
    pub project_id: Option<Uuid>,
    pub for_seconds: i32,
}

/// Pure state machine for alert transitions. Returns what actions to take,
/// and mutates `state` in place.
pub fn next_alert_state(
    state: &mut AlertState,
    condition_met: bool,
    now: DateTime<Utc>,
    for_seconds: i32,
) -> AlertTransition {
    if condition_met {
        if state.first_triggered.is_none() {
            state.first_triggered = Some(now);
        }
        // Safety: first_triggered is guaranteed Some — set above
        let triggered_at = state.first_triggered.expect("set to Some above");
        let held_for = (now - triggered_at).num_seconds();
        if held_for >= i64::from(for_seconds) && !state.firing {
            state.firing = true;
            return AlertTransition {
                should_fire: true,
                should_resolve: false,
            };
        }
        AlertTransition {
            should_fire: false,
            should_resolve: false,
        }
    } else {
        let was_firing = state.firing;
        state.first_triggered = None;
        state.firing = false;
        AlertTransition {
            should_fire: false,
            should_resolve: was_firing,
        }
    }
}

/// Check whether a condition is met given the threshold and value.
pub fn check_condition(condition: &str, threshold: Option<f64>, value: Option<f64>) -> bool {
    match condition {
        "absent" => value.is_none(),
        "gt" => value.is_some_and(|v| threshold.is_some_and(|t| v > t)),
        "lt" => value.is_some_and(|v| threshold.is_some_and(|t| v < t)),
        "eq" => value.is_some_and(|v| threshold.is_some_and(|t| (v - t).abs() < f64::EPSILON)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// DB operations
// ---------------------------------------------------------------------------

/// Query metric samples for alert evaluation.
pub async fn evaluate_metric(
    pool: &sqlx::PgPool,
    name: &str,
    labels: Option<&serde_json::Value>,
    agg: &str,
    window_secs: i32,
) -> Result<Option<f64>, sqlx::Error> {
    match agg {
        "avg" => {
            sqlx::query_scalar!(
                r#"SELECT AVG(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3 * interval '1 second'"#,
                name,
                labels,
                f64::from(window_secs),
            )
            .fetch_one(pool)
            .await
        }
        "sum" => {
            sqlx::query_scalar!(
                r#"SELECT SUM(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3 * interval '1 second'"#,
                name,
                labels,
                f64::from(window_secs),
            )
            .fetch_one(pool)
            .await
        }
        "max" => {
            sqlx::query_scalar!(
                r#"SELECT MAX(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3 * interval '1 second'"#,
                name,
                labels,
                f64::from(window_secs),
            )
            .fetch_one(pool)
            .await
        }
        "min" => {
            sqlx::query_scalar!(
                r#"SELECT MIN(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3 * interval '1 second'"#,
                name,
                labels,
                f64::from(window_secs),
            )
            .fetch_one(pool)
            .await
        }
        "count" => {
            let count: Option<i64> = sqlx::query_scalar!(
                r#"SELECT COUNT(ms.value) FROM metric_samples ms
                   JOIN metric_series ser ON ser.id = ms.series_id
                   WHERE ser.name = $1 AND ($2::jsonb IS NULL OR ser.labels @> $2)
                   AND ms.timestamp > now() - $3 * interval '1 second'"#,
                name,
                labels,
                f64::from(window_secs),
            )
            .fetch_one(pool)
            .await?;
            #[allow(clippy::cast_precision_loss)]
            Ok(count.map(|c| c as f64))
        }
        _ => Ok(None),
    }
}

/// Insert a "firing" alert event.
pub async fn fire_alert(
    pool: &sqlx::PgPool,
    rule_id: Uuid,
    value: Option<f64>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"INSERT INTO alert_events (rule_id, status, value, message)
        VALUES ($1, 'firing', $2, 'Alert condition met')"#,
        rule_id,
        value,
    )
    .execute(pool)
    .await?;

    tracing::warn!(rule_id = %rule_id, ?value, "alert firing");
    Ok(())
}

/// Resolve the most recent firing event for this rule.
pub async fn resolve_alert(pool: &sqlx::PgPool, rule_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"UPDATE alert_events SET status = 'resolved', resolved_at = now()
        WHERE rule_id = $1 AND status = 'firing' AND resolved_at IS NULL"#,
        rule_id,
    )
    .execute(pool)
    .await?;

    tracing::info!(rule_id = %rule_id, "alert resolved");
    Ok(())
}

/// Handle alert state transition with explicit pool/valkey params.
///
/// Publishes to a dedicated `"alert:fired"` Valkey channel (not `PlatformEvent`).
/// The main binary's eventbus subscribes to this channel separately.
pub async fn handle_alert_state(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    condition_met: bool,
    value: Option<f64>,
    now: DateTime<Utc>,
    alert_state: &mut AlertState,
    rule_info: &AlertRuleInfo<'_>,
) {
    let transition = next_alert_state(alert_state, condition_met, now, rule_info.for_seconds);
    if transition.should_fire {
        if let Err(e) = fire_alert(pool, rule_info.id, value).await {
            tracing::error!(error = %e, rule_id = %rule_info.id, "failed to persist alert firing");
        }
        // Publish to dedicated "alert:fired" channel
        let payload = serde_json::json!({
            "rule_id": rule_info.id,
            "project_id": rule_info.project_id,
            "severity": rule_info.severity,
            "value": value,
            "alert_name": rule_info.name,
        });
        let _ = platform_types::valkey::publish(valkey, "alert:fired", &payload.to_string()).await;
    }
    if transition.should_resolve
        && let Err(e) = resolve_alert(pool, rule_info.id).await
    {
        tracing::error!(error = %e, rule_id = %rule_info.id, "failed to resolve alert");
    }
}

// ---------------------------------------------------------------------------
// Evaluation loop — background task
// ---------------------------------------------------------------------------

/// Run the alert evaluation loop until shutdown.
pub async fn evaluate_alerts_loop(
    pool: sqlx::PgPool,
    valkey: fred::clients::Pool,
    cancel: tokio_util::sync::CancellationToken,
) {
    tracing::info!("alert evaluator started");
    let mut alert_states: std::collections::HashMap<Uuid, AlertState> =
        std::collections::HashMap::new();

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("alert evaluator shutting down");
                break;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                match evaluate_all(&pool, &valkey, &mut alert_states).await {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "alert evaluation cycle failed");
                    }
                }
            }
        }
    }
}

#[allow(clippy::implicit_hasher)]
async fn evaluate_all(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    alert_states: &mut std::collections::HashMap<Uuid, AlertState>,
) -> Result<(), anyhow::Error> {
    use sqlx::Row;

    let rules = sqlx::query(
        "SELECT id, name, query, condition, threshold, for_seconds, severity, project_id \
         FROM alert_rules WHERE enabled = true ORDER BY id LIMIT 500",
    )
    .fetch_all(pool)
    .await?;

    if rules.len() >= 500 {
        tracing::warn!("alert rule limit reached (500) — some rules may not be evaluated");
    }

    let rule_timeout = std::time::Duration::from_secs(10);
    for rule in &rules {
        let rule_id: Uuid = rule.get("id");
        let rule_name: String = rule.get("name");

        match tokio::time::timeout(
            rule_timeout,
            evaluate_one_rule(pool, valkey, alert_states, rule),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    rule_id = %rule_id, rule_name = %rule_name,
                    error = %e, "alert rule evaluation failed"
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    rule_id = %rule_id, rule_name = %rule_name,
                    "alert rule evaluation timed out (10s)"
                );
            }
        }
    }

    Ok(())
}

async fn evaluate_one_rule(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    alert_states: &mut std::collections::HashMap<Uuid, AlertState>,
    rule: &sqlx::postgres::PgRow,
) -> Result<(), anyhow::Error> {
    use sqlx::Row;

    let rule_id: Uuid = rule.get("id");
    let rule_name: String = rule.get("name");
    let rule_query: String = rule.get("query");
    let rule_condition: String = rule.get("condition");
    let rule_threshold: Option<f64> = rule.get("threshold");
    let rule_for_seconds: i32 = rule.get("for_seconds");
    let rule_severity: String = rule.get("severity");
    let rule_project_id: Option<Uuid> = rule.get("project_id");

    let aq = parse_alert_query(&rule_query)?;

    let value = evaluate_metric(
        pool,
        &aq.metric_name,
        aq.labels.as_ref(),
        &aq.aggregation,
        aq.window_secs,
    )
    .await?;

    let condition_met = check_condition(&rule_condition, rule_threshold, value);

    let now = Utc::now();
    let as_entry = alert_states.entry(rule_id).or_insert(AlertState {
        first_triggered: None,
        firing: false,
    });

    let rule_info = AlertRuleInfo {
        id: rule_id,
        name: &rule_name,
        severity: &rule_severity,
        project_id: rule_project_id,
        for_seconds: rule_for_seconds,
    };
    handle_alert_state(
        pool,
        valkey,
        condition_met,
        value,
        now,
        as_entry,
        &rule_info,
    )
    .await;

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

    #[test]
    fn condition_gt_no_value_returns_false() {
        assert!(!check_condition("gt", Some(10.0), None));
    }

    #[test]
    fn condition_gt_no_threshold_returns_false() {
        assert!(!check_condition("gt", None, Some(15.0)));
    }

    #[test]
    fn condition_lt_no_value_returns_false() {
        assert!(!check_condition("lt", Some(10.0), None));
    }

    #[test]
    fn condition_eq_no_value_returns_false() {
        assert!(!check_condition("eq", Some(10.0), None));
    }

    #[test]
    fn condition_unknown_returns_false() {
        assert!(!check_condition("unknown", Some(10.0), Some(15.0)));
        assert!(!check_condition("", Some(10.0), Some(15.0)));
    }

    #[test]
    fn condition_eq_near_epsilon() {
        let v = 10.0;
        let close = v + f64::EPSILON * 0.5;
        assert!(check_condition("eq", Some(v), Some(close)));
    }

    #[test]
    fn condition_nan_returns_false() {
        assert!(!check_condition("gt", Some(10.0), Some(f64::NAN)));
        assert!(!check_condition("lt", Some(10.0), Some(f64::NAN)));
        assert!(!check_condition("eq", Some(10.0), Some(f64::NAN)));
    }

    #[test]
    fn condition_infinity_gt_threshold() {
        assert!(check_condition("gt", Some(10.0), Some(f64::INFINITY)));
    }

    // -- validate_condition --

    #[test]
    fn validate_condition_valid_values() {
        assert!(validate_condition("gt").is_ok());
        assert!(validate_condition("lt").is_ok());
        assert!(validate_condition("eq").is_ok());
        assert!(validate_condition("absent").is_ok());
    }

    #[test]
    fn validate_condition_invalid_values() {
        assert!(validate_condition("gte").is_err());
        assert!(validate_condition("").is_err());
        assert!(validate_condition("GT").is_err());
    }

    // -- parse_alert_query edge cases --

    #[test]
    fn parse_query_window_at_min_boundary() {
        let q = parse_alert_query("metric:cpu window:10").unwrap();
        assert_eq!(q.window_secs, 10);
    }

    #[test]
    fn parse_query_window_at_max_boundary() {
        let q = parse_alert_query("metric:cpu window:86400").unwrap();
        assert_eq!(q.window_secs, 86400);
    }

    #[test]
    fn parse_query_window_below_min_rejected() {
        assert!(parse_alert_query("metric:cpu window:9").is_err());
    }

    #[test]
    fn parse_query_window_above_max_rejected() {
        assert!(parse_alert_query("metric:cpu window:86401").is_err());
    }

    #[test]
    fn parse_query_window_non_integer_rejected() {
        assert!(parse_alert_query("metric:cpu window:abc").is_err());
    }

    #[test]
    fn parse_query_all_aggregations() {
        for agg in &["avg", "sum", "max", "min", "count"] {
            let q = parse_alert_query(&format!("metric:cpu agg:{agg}")).unwrap();
            assert_eq!(q.aggregation, *agg);
        }
    }

    #[test]
    fn parse_query_empty_rejected() {
        assert!(parse_alert_query("").is_err());
    }

    #[test]
    fn parse_query_invalid_labels_json() {
        assert!(parse_alert_query("metric:cpu labels:not-json").is_err());
    }

    // -- alert state machine (next_alert_state) --

    #[test]
    fn alert_inactive_to_pending_on_condition_met() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: None,
            firing: false,
        };
        let t = next_alert_state(&mut state, true, now, 60);
        assert!(state.first_triggered.is_some());
        assert!(!state.firing);
        assert!(!t.should_fire);
        assert!(!t.should_resolve);
    }

    #[test]
    fn alert_pending_to_firing_after_hold_period() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: Some(now - chrono::Duration::seconds(120)),
            firing: false,
        };
        let t = next_alert_state(&mut state, true, now, 60);
        assert!(state.firing);
        assert!(t.should_fire);
        assert!(!t.should_resolve);
    }

    #[test]
    fn alert_pending_resets_when_condition_clears() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: Some(now - chrono::Duration::seconds(30)),
            firing: false,
        };
        let t = next_alert_state(&mut state, false, now, 60);
        assert!(state.first_triggered.is_none());
        assert!(!state.firing);
        assert!(!t.should_fire);
        assert!(!t.should_resolve);
    }

    #[test]
    fn alert_firing_resolves_when_condition_clears() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: Some(now - chrono::Duration::seconds(300)),
            firing: true,
        };
        let t = next_alert_state(&mut state, false, now, 60);
        assert!(!state.firing);
        assert!(state.first_triggered.is_none());
        assert!(!t.should_fire);
        assert!(t.should_resolve);
    }

    #[test]
    fn alert_firing_stays_firing_while_condition_holds() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: Some(now - chrono::Duration::seconds(300)),
            firing: true,
        };
        let t = next_alert_state(&mut state, true, now, 60);
        assert!(state.firing);
        assert!(!t.should_fire);
        assert!(!t.should_resolve);
    }

    #[test]
    fn alert_already_firing_no_duplicate_notification() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: Some(now - chrono::Duration::seconds(600)),
            firing: true,
        };
        for _ in 0..5 {
            let t = next_alert_state(&mut state, true, now, 60);
            assert!(!t.should_fire);
        }
    }

    #[test]
    fn parse_query_multiple_spaces_between_parts() {
        let q = parse_alert_query("metric:cpu_usage   agg:sum   window:600").unwrap();
        assert_eq!(q.metric_name, "cpu_usage");
        assert_eq!(q.aggregation, "sum");
        assert_eq!(q.window_secs, 600);
    }

    #[test]
    fn parse_query_metric_only_uses_defaults() {
        let q = parse_alert_query("metric:memory_usage").unwrap();
        assert_eq!(q.metric_name, "memory_usage");
        assert_eq!(q.aggregation, "avg");
        assert_eq!(q.window_secs, 300);
        assert!(q.labels.is_none());
    }

    #[test]
    fn parse_query_labels_valid_json_object() {
        let q =
            parse_alert_query(r#"metric:errors labels:{"env":"prod","service":"api"} agg:count"#)
                .unwrap();
        assert_eq!(q.aggregation, "count");
        let labels = q.labels.unwrap();
        assert_eq!(labels["env"], "prod");
        assert_eq!(labels["service"], "api");
    }

    #[test]
    fn parse_query_labels_array_json() {
        let q = parse_alert_query(r"metric:test labels:[1,2,3]").unwrap();
        let labels = q.labels.unwrap();
        assert!(labels.is_array());
    }

    #[test]
    fn parse_query_too_long_rejected() {
        let long_query = format!("metric:{}", "x".repeat(1001));
        assert!(parse_alert_query(&long_query).is_err());
    }

    #[test]
    fn parse_query_unknown_prefix_ignored() {
        let q = parse_alert_query("metric:cpu foo:bar baz:qux").unwrap();
        assert_eq!(q.metric_name, "cpu");
        assert_eq!(q.aggregation, "avg");
    }

    #[test]
    fn condition_eq_both_none_returns_false() {
        assert!(!check_condition("eq", None, None));
    }

    #[test]
    fn condition_lt_equal_values_returns_false() {
        assert!(!check_condition("lt", Some(10.0), Some(10.0)));
    }

    #[test]
    fn condition_gt_equal_values_returns_false() {
        assert!(!check_condition("gt", Some(10.0), Some(10.0)));
    }

    #[test]
    fn condition_absent_with_some_threshold_returns_false() {
        assert!(!check_condition("absent", Some(10.0), Some(5.0)));
    }

    #[test]
    fn condition_gt_negative_values() {
        assert!(check_condition("gt", Some(-10.0), Some(-5.0)));
        assert!(!check_condition("gt", Some(-5.0), Some(-10.0)));
    }

    #[test]
    fn alert_state_pending_exactly_at_hold_period() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: Some(now - chrono::Duration::seconds(60)),
            firing: false,
        };
        let t = next_alert_state(&mut state, true, now, 60);
        assert!(state.firing);
        assert!(t.should_fire);
    }

    #[test]
    fn alert_state_with_zero_hold_period() {
        let now = Utc::now();
        let mut state = AlertState {
            first_triggered: None,
            firing: false,
        };
        let t = next_alert_state(&mut state, true, now, 0);
        assert!(state.firing);
        assert!(t.should_fire);
    }

    #[test]
    fn validate_condition_whitespace_rejected() {
        assert!(validate_condition(" gt ").is_err());
    }
}
