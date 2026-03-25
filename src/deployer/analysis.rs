//! Background analysis loop for canary/AB releases.
//!
//! Runs every 15 seconds, evaluates progress gates and rollback triggers
//! for active releases, writes verdicts to `rollout_analyses` table.
//! The reconciler reads verdicts on its next tick.

use sqlx::Row;
use tracing::Instrument;
use uuid::Uuid;

use crate::store::AppState;

use super::types::{AnalysisVerdict, MetricGate};

/// Background task: evaluate metrics for progressing/holding releases.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("analysis loop started");
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    state.task_registry.register("analysis_loop", 30);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("analysis loop shutting down");
                break;
            }
            _ = interval.tick() => {
                state.task_registry.heartbeat("analysis_loop");
                let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                let span = tracing::info_span!(
                    "task_iteration",
                    task_name = "analysis_loop",
                    trace_id = %iter_trace_id,
                    source = "system",
                );
                async {
                    if let Err(e) = tick(&state).await {
                        state.task_registry.report_error("analysis_loop", &e.to_string());
                        tracing::error!(error = %e, "analysis tick failed");
                    }
                }
                .instrument(span)
                .await;
            }
        }
    }
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    // Find releases that need analysis: progressing or holding with canary/ab_test strategy
    let releases = sqlx::query(
        "SELECT id, project_id, strategy, current_step, rollout_config, analysis_config
         FROM deploy_releases
         WHERE phase IN ('progressing', 'holding')
           AND strategy IN ('canary', 'ab_test')",
    )
    .fetch_all(&state.pool)
    .await?;

    for release in &releases {
        let release_id: Uuid = release.get("id");
        let project_id: Uuid = release.get("project_id");
        let step_index: i32 = release.get("current_step");
        let rollout_config: serde_json::Value = release.get("rollout_config");

        let span = tracing::info_span!("analyze_release", %release_id, %project_id);
        async {
            if let Err(e) =
                analyze_release(state, release_id, project_id, step_index, &rollout_config).await
            {
                tracing::error!(error = %e, "release analysis failed");
            }
        }
        .instrument(span)
        .await;
    }

    Ok(())
}

/// Evaluate a single release's metrics against configured gates.
async fn analyze_release(
    state: &AppState,
    release_id: Uuid,
    project_id: Uuid,
    step_index: i32,
    rollout_config: &serde_json::Value,
) -> anyhow::Result<()> {
    let analysis_id = ensure_analysis_record(state, release_id, step_index, rollout_config).await?;

    // Check rollback triggers first (instant rollback on breach)
    let rollback_triggers: Vec<MetricGate> = rollout_config
        .get("rollback_triggers")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    if let Some(fail_result) =
        check_rollback_triggers(state, project_id, &rollback_triggers).await?
    {
        complete_analysis(
            &state.pool,
            analysis_id,
            AnalysisVerdict::Fail,
            &fail_result,
        )
        .await?;
        return Ok(());
    }

    // Evaluate progress gates
    let verdict = evaluate_progress_gates(state, project_id, rollout_config).await?;

    complete_analysis(
        &state.pool,
        analysis_id,
        verdict.0,
        &serde_json::json!({ "gates": verdict.1 }),
    )
    .await?;

    Ok(())
}

/// Ensure a running analysis record exists, return its ID.
async fn ensure_analysis_record(
    state: &AppState,
    release_id: Uuid,
    step_index: i32,
    rollout_config: &serde_json::Value,
) -> anyhow::Result<Uuid> {
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM rollout_analyses
         WHERE release_id = $1 AND step_index = $2 AND verdict = 'running'",
    )
    .bind(release_id)
    .bind(step_index)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(id) = existing {
        return Ok(id);
    }

    let config = serde_json::json!({
        "step_index": step_index,
        "rollout_config": rollout_config,
    });
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO rollout_analyses (release_id, step_index, config, verdict)
         VALUES ($1, $2, $3, 'running')
         RETURNING id",
    )
    .bind(release_id)
    .bind(step_index)
    .bind(&config)
    .fetch_one(&state.pool)
    .await?;

    Ok(id)
}

/// Check rollback triggers — returns `Some(fail_detail)` if any breached.
async fn check_rollback_triggers(
    state: &AppState,
    project_id: Uuid,
    triggers: &[MetricGate],
) -> anyhow::Result<Option<serde_json::Value>> {
    for trigger in triggers {
        let value = evaluate_gate_metric(state, project_id, trigger).await?;
        if let Some(val) = value
            && crate::observe::alert::check_condition(
                &trigger.condition,
                Some(trigger.threshold),
                Some(val),
            )
        {
            return Ok(Some(serde_json::json!({
                "reason": "rollback_trigger_breached",
                "metric": trigger.metric,
                "value": val,
                "threshold": trigger.threshold,
            })));
        }
    }
    Ok(None)
}

/// Evaluate all progress gates, return (verdict, results).
async fn evaluate_progress_gates(
    state: &AppState,
    project_id: Uuid,
    rollout_config: &serde_json::Value,
) -> anyhow::Result<(AnalysisVerdict, Vec<serde_json::Value>)> {
    let progress_gates: Vec<MetricGate> = rollout_config
        .get("progress_gates")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Check min_requests threshold before evaluating any gates.
    // If traffic volume is too low, return Inconclusive — the reconciler will wait.
    let min_requests = rollout_config
        .get("min_requests")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(100);

    if min_requests > 0 {
        let request_count = count_project_requests(state, project_id).await;
        if request_count < min_requests {
            return Ok((
                AnalysisVerdict::Inconclusive,
                vec![serde_json::json!({
                    "reason": "insufficient_traffic",
                    "min_requests": min_requests,
                    "actual_requests": request_count,
                })],
            ));
        }
    }

    let mut all_pass = true;
    let mut results = Vec::new();
    for gate in &progress_gates {
        let value = evaluate_gate_metric(state, project_id, gate).await?;
        let passed = match value {
            Some(val) => !crate::observe::alert::check_condition(
                &invert_condition(&gate.condition),
                Some(gate.threshold),
                Some(val),
            ),
            None => false,
        };
        results.push(serde_json::json!({
            "metric": gate.metric,
            "value": value,
            "threshold": gate.threshold,
            "condition": gate.condition,
            "passed": passed,
        }));
        if !passed {
            all_pass = false;
        }
    }

    let verdict = if progress_gates.is_empty() || all_pass {
        AnalysisVerdict::Pass
    } else {
        AnalysisVerdict::Fail
    };

    Ok((verdict, results))
}

/// Count total HTTP requests for a project in the recent time window.
///
/// Queries the metrics store for any metric with the project's label.
/// Returns 0 on error (fail-open: analysis treats as "no traffic yet").
async fn count_project_requests(state: &AppState, project_id: Uuid) -> u64 {
    let labels = serde_json::json!({
        "platform.project_id": project_id.to_string()
    });

    // Use a broad metric name and sum aggregation over 5-minute window
    let result = crate::observe::alert::evaluate_metric(
        &state.pool,
        "http_requests_total",
        Some(&labels),
        "sum",
        300, // 5 min window
    )
    .await;

    match result {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(Some(val)) => val as u64,
        _ => 0, // No data or error → treat as zero requests
    }
}

/// Query a metric value based on the gate config, scoped to the project.
async fn evaluate_gate_metric(
    state: &AppState,
    project_id: Uuid,
    gate: &MetricGate,
) -> anyhow::Result<Option<f64>> {
    let metric_name = if gate.metric == "custom" {
        gate.name.as_deref().unwrap_or("")
    } else {
        &gate.metric
    };

    // Scope metric queries to this project via OTEL resource attribute label
    let labels = serde_json::json!({
        "platform.project_id": project_id.to_string()
    });

    let value = crate::observe::alert::evaluate_metric(
        &state.pool,
        metric_name,
        Some(&labels),
        &gate.aggregation,
        gate.window,
    )
    .await?;

    Ok(value)
}

/// Invert a condition for progress gate checking.
fn invert_condition(condition: &str) -> String {
    match condition {
        "lt" => "gt".to_string(),
        "gt" => "lt".to_string(),
        other => other.to_string(),
    }
}

async fn complete_analysis(
    pool: &sqlx::PgPool,
    analysis_id: Uuid,
    verdict: AnalysisVerdict,
    metric_results: &serde_json::Value,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE rollout_analyses SET verdict = $2, metric_results = $3, completed_at = now()
         WHERE id = $1",
    )
    .bind(analysis_id)
    .bind(verdict.as_str())
    .bind(metric_results)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invert_condition_lt_to_gt() {
        assert_eq!(invert_condition("lt"), "gt");
    }

    #[test]
    fn invert_condition_gt_to_lt() {
        assert_eq!(invert_condition("gt"), "lt");
    }

    #[test]
    fn invert_condition_eq_unchanged() {
        assert_eq!(invert_condition("eq"), "eq");
    }

    #[test]
    fn inconclusive_verdict_is_not_terminal() {
        assert!(!AnalysisVerdict::Inconclusive.is_terminal());
    }

    #[test]
    fn inconclusive_verdict_as_str() {
        assert_eq!(AnalysisVerdict::Inconclusive.as_str(), "inconclusive");
    }
}
