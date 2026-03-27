use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::Secret;
use kube::Api;
use kube::api::PostParams;
use sqlx::Row;
use tracing::Instrument;
use uuid::Uuid;

use crate::store::AppState;

use super::error::DeployerError;
use super::{applier, ops_repo, renderer};

/// Fixed name for the registry pull secret created in every project namespace.
pub const REGISTRY_PULL_SECRET_NAME: &str = "platform-registry-pull";

// ---------------------------------------------------------------------------
// Background reconciliation loop
// ---------------------------------------------------------------------------

/// Background task that polls for pending releases and reconciles them.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("deployer reconciler started");

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    state.task_registry.register("deployer_reconciler", 15);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("deployer reconciler shutting down");
                break;
            }
            _ = interval.tick() => {
                let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                let span = tracing::info_span!(
                    "task_iteration",
                    task_name = "deployer_reconciler",
                    trace_id = %iter_trace_id,
                    source = "system",
                );
                async {
                    match reconcile(&state).await {
                        Ok(()) => state.task_registry.heartbeat("deployer_reconciler"),
                        Err(e) => {
                            state.task_registry.report_error("deployer_reconciler", &e.to_string());
                            tracing::error!(error = %e, "error polling pending releases");
                        }
                    }
                }.instrument(span).await;
            }
            () = state.deploy_notify.notified() => {
                let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                let span = tracing::info_span!(
                    "task_iteration",
                    task_name = "deployer_reconciler",
                    trace_id = %iter_trace_id,
                    source = "system",
                );
                async {
                    if let Err(e) = reconcile(&state).await {
                        tracing::error!(error = %e, "error polling pending releases (notified)");
                    }
                }.instrument(span).await;
                interval.reset();
            }
        }
    }
}

/// Find releases needing reconciliation and spawn tasks for each.
async fn reconcile(state: &AppState) -> Result<(), DeployerError> {
    let pending = sqlx::query(
        "SELECT r.id, r.target_id, r.project_id, r.image_ref, r.commit_sha,
                r.strategy, r.phase, r.traffic_weight, r.current_step,
                r.rollout_config, r.values_override, r.deployed_by,
                r.tracked_resources, r.pipeline_id,
                dt.environment, dt.ops_repo_id, dt.manifest_path, dt.branch_slug,
                p.name as project_name, p.namespace_slug
         FROM deploy_releases r
         JOIN deploy_targets dt ON dt.id = r.target_id
         JOIN projects p ON p.id = r.project_id AND p.is_active = true
         WHERE r.phase IN ('pending','progressing','holding','promoting','rolling_back')
         ORDER BY r.created_at ASC
         LIMIT 10",
    )
    .fetch_all(&state.pool)
    .await?;

    for row in &pending {
        let release_id: Uuid = row.get("id");
        let (tracked, skip_prune) = match serde_json::from_value::<Vec<applier::TrackedResource>>(
            row.get("tracked_resources"),
        ) {
            Ok(t) => (t, false),
            Err(e) => {
                tracing::warn!(%release_id, error = %e, "failed to parse tracked_resources");
                (Vec::new(), true)
            }
        };

        let release = PendingRelease {
            id: release_id,
            target_id: row.get("target_id"),
            project_id: row.get("project_id"),
            image_ref: row.get("image_ref"),
            commit_sha: row.get("commit_sha"),
            strategy: row.get("strategy"),
            phase: row.get("phase"),
            traffic_weight: row.get("traffic_weight"),
            current_step: row.get("current_step"),
            rollout_config: row.get("rollout_config"),
            values_override: row.get("values_override"),
            deployed_by: row.get("deployed_by"),
            pipeline_id: row.get("pipeline_id"),
            environment: row.get("environment"),
            ops_repo_id: row.get("ops_repo_id"),
            manifest_path: row.get("manifest_path"),
            branch_slug: row.get("branch_slug"),
            project_name: row.get("project_name"),
            namespace_slug: row.get("namespace_slug"),
            tracked_resources: tracked,
            skip_prune,
        };

        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = reconcile_one(&state, &release).await {
                tracing::error!(error = %e, release_id = %release.id, "reconciliation failed");
                mark_failed(&state, &release, &e.to_string()).await;
            }
        });
    }

    // Cleanup expired preview targets
    cleanup_expired_previews(state).await;

    Ok(())
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct PendingRelease {
    pub id: Uuid,
    pub target_id: Uuid,
    pub project_id: Uuid,
    pub image_ref: String,
    pub commit_sha: Option<String>,
    pub strategy: String,
    pub phase: String,
    pub traffic_weight: i32,
    pub current_step: i32,
    pub rollout_config: serde_json::Value,
    pub values_override: Option<serde_json::Value>,
    pub deployed_by: Option<Uuid>,
    pub pipeline_id: Option<Uuid>,
    // From deploy_targets
    pub environment: String,
    pub ops_repo_id: Option<Uuid>,
    pub manifest_path: Option<String>,
    pub branch_slug: Option<String>,
    // From projects
    pub project_name: String,
    pub namespace_slug: String,
    // Parsed
    pub tracked_resources: Vec<applier::TrackedResource>,
    pub skip_prune: bool,
}

/// Map environment name to K8s-safe suffix.
fn env_suffix(environment: &str) -> &str {
    match environment {
        "production" => "prod",
        other => other,
    }
}

/// Resolve the target K8s namespace for a deployment.
pub fn target_namespace(
    config: &crate::config::Config,
    namespace_slug: &str,
    environment: &str,
) -> String {
    config.project_namespace(namespace_slug, env_suffix(environment))
}

// ---------------------------------------------------------------------------
// Single release reconciliation
// ---------------------------------------------------------------------------

/// Claim and reconcile a single release. Uses optimistic locking.
async fn reconcile_one(state: &AppState, release: &PendingRelease) -> Result<(), DeployerError> {
    // Claim with optimistic lock — only process if still in expected phase
    let claimed = sqlx::query_scalar::<_, Uuid>(
        "UPDATE deploy_releases SET started_at = COALESCE(started_at, now())
         WHERE id = $1 AND phase = $2
         RETURNING id",
    )
    .bind(release.id)
    .bind(&release.phase)
    .fetch_optional(&state.pool)
    .await?;

    if claimed.is_none() {
        tracing::debug!(release_id = %release.id, "release phase changed, skipping");
        return Ok(());
    }

    match release.phase.as_str() {
        "pending" => handle_pending(state, release).await,
        "progressing" | "holding" => match release.strategy.as_str() {
            "rolling" => handle_rolling_progress(state, release),
            "canary" => handle_canary_progress(state, release).await,
            "ab_test" => handle_ab_test_progress(state, release).await,
            _ => {
                tracing::warn!(release_id = %release.id, strategy = %release.strategy, "unknown strategy");
                Ok(())
            }
        },
        "promoting" => handle_promoting(state, release).await,
        "rolling_back" => handle_rolling_back(state, release).await,
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Phase handlers
// ---------------------------------------------------------------------------

/// Pending release — apply manifests, transition to progressing.
async fn handle_pending(state: &AppState, release: &PendingRelease) -> Result<(), DeployerError> {
    let ns = target_namespace(&state.config, &release.namespace_slug, &release.environment);

    // Ensure namespace, secrets, registry pull secret
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns,
        env_suffix(&release.environment),
        &release.project_id.to_string(),
        &state.config.platform_namespace,
        false,
    )
    .await?;

    let secrets_name = inject_project_secrets(state, release, &ns).await;
    ensure_registry_pull_secret_for(state, release.project_id, release.id, &ns).await;

    // Render + apply manifests
    let (rendered, _sha) = render_manifests(state, release).await?;
    let rendered = if let Some(ref sn) = secrets_name {
        applier::inject_env_from_secret(&rendered, sn)?
    } else {
        rendered
    };

    // Prune orphans
    let new_tracked = applier::build_tracked_inventory(&rendered, &ns);
    if !release.skip_prune {
        let orphans = applier::find_orphans(&release.tracked_resources, &new_tracked);
        if !orphans.is_empty() {
            let pruned = applier::prune_orphans(&state.kube, &orphans).await?;
            tracing::info!(pruned_count = pruned, "orphaned resources pruned");
        }
    }

    let applied =
        applier::apply_with_tracking(&state.kube, &rendered, &ns, Some(release.id)).await?;
    store_tracked_resources(state, release.id, &new_tracked).await?;

    // For rolling strategy: wait for health and complete immediately
    if release.strategy == "rolling" {
        if let Some(deploy_name) = applier::find_deployment_name(&applied) {
            applier::wait_healthy(&state.kube, &ns, deploy_name, Duration::from_secs(300)).await?;
        }
        transition_phase(state, release, "completed", Some(100), Some("healthy")).await?;
        record_history(state, release, "promoted", "completed", Some(100)).await;
        fire_webhook(state, release, "deployed").await;

        let _ = crate::store::eventbus::publish(
            &state.valkey,
            &crate::store::eventbus::PlatformEvent::ReleasePromoted {
                release_id: release.id,
                project_id: release.project_id,
                image_ref: release.image_ref.clone(),
            },
        )
        .await;
    } else {
        // Canary/AB: wait for canary deployment health before traffic switch
        if let Some(deploy_name) = applier::find_deployment_name(&applied) {
            applier::wait_healthy(&state.kube, &ns, deploy_name, Duration::from_secs(300)).await?;
        }

        // Re-check phase — release may have been cancelled while waiting for health
        let current_phase: Option<String> =
            sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
                .bind(release.id)
                .fetch_optional(&state.pool)
                .await?;
        if current_phase.as_deref() != Some("pending") {
            tracing::info!(release_id = %release.id, phase = ?current_phase, "release phase changed during health wait, aborting");
            return Ok(());
        }

        // Canary/AB: transition to progressing with initial weight
        #[allow(clippy::cast_possible_truncation)]
        let initial_weight = release
            .rollout_config
            .get("steps")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0) as i32;

        // Set initial weight in DB
        sqlx::query("UPDATE deploy_releases SET traffic_weight = $2 WHERE id = $1")
            .bind(release.id)
            .bind(initial_weight)
            .execute(&state.pool)
            .await?;

        // Apply gateway resources for traffic splitting
        apply_gateway_resources(state, release, &ns, initial_weight).await;

        transition_phase(state, release, "progressing", Some(initial_weight), None).await?;
        record_history(
            state,
            release,
            "step_advanced",
            "progressing",
            Some(initial_weight),
        )
        .await;
    }

    Ok(())
}

/// Rolling release already progressing — nothing to do, should have completed in pending.
#[allow(clippy::unnecessary_wraps)]
fn handle_rolling_progress(
    _state: &AppState,
    _release: &PendingRelease,
) -> Result<(), DeployerError> {
    // Rolling deploys complete in handle_pending. If we're here, it's a no-op.
    Ok(())
}

/// Canary release progressing — check analysis verdicts and step forward.
async fn handle_canary_progress(
    state: &AppState,
    release: &PendingRelease,
) -> Result<(), DeployerError> {
    // Check latest analysis verdict for current step
    let verdict = sqlx::query_scalar::<_, String>(
        "SELECT verdict FROM rollout_analyses
         WHERE release_id = $1 AND step_index = $2
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(release.id)
    .bind(release.current_step)
    .fetch_optional(&state.pool)
    .await?;

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    match verdict.as_deref() {
        Some("pass") => {
            // Advance to next step
            let steps: Vec<i32> = release
                .rollout_config
                .get("steps")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            let next_step = release.current_step + 1;
            if usize::try_from(next_step).unwrap_or(usize::MAX) >= steps.len() {
                // All steps passed — promote
                transition_phase(state, release, "promoting", Some(100), None).await?;
                record_history(state, release, "promoted", "promoting", Some(100)).await;
            } else {
                let weight = steps.get(next_step as usize).copied().unwrap_or(100);
                sqlx::query(
                    "UPDATE deploy_releases SET current_step = $2, traffic_weight = $3 WHERE id = $1",
                )
                .bind(release.id)
                .bind(next_step)
                .bind(weight)
                .execute(&state.pool)
                .await?;

                // Update HTTPRoute with new weight
                let ns =
                    target_namespace(&state.config, &release.namespace_slug, &release.environment);
                apply_gateway_resources(state, release, &ns, weight).await;

                record_history(state, release, "step_advanced", "progressing", Some(weight)).await;
            }
        }
        Some("fail") => {
            let config = &release.rollout_config;
            let max_failures: i64 = config
                .get("max_failures")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(3);

            let fail_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM rollout_analyses
                 WHERE release_id = $1 AND step_index = $2 AND verdict = 'fail'",
            )
            .bind(release.id)
            .bind(release.current_step)
            .fetch_one(&state.pool)
            .await?;

            if fail_count >= max_failures {
                transition_phase(state, release, "rolling_back", Some(0), Some("unhealthy"))
                    .await?;
                record_history(state, release, "rolled_back", "rolling_back", Some(0)).await;
            } else if release.phase == "progressing" {
                transition_phase(state, release, "holding", None, Some("degraded")).await?;
                record_history(state, release, "health_changed", "holding", None).await;
            }
        }
        Some("inconclusive") => {
            // Insufficient traffic — wait for more data, don't count as failure
            tracing::info!(release_id = %release.id, "analysis inconclusive, waiting for traffic");
        }
        // Analysis still running or not started yet — wait
        _ => {}
    }

    Ok(())
}

/// A/B test progressing — similar to canary but with duration-based completion.
async fn handle_ab_test_progress(
    state: &AppState,
    release: &PendingRelease,
) -> Result<(), DeployerError> {
    let duration_secs: i64 = release
        .rollout_config
        .get("duration")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(86400);

    // Check if duration has elapsed since started_at
    let elapsed: Option<bool> = sqlx::query_scalar(
        "SELECT (now() - started_at) > ($2 || ' seconds')::interval
         FROM deploy_releases WHERE id = $1 AND started_at IS NOT NULL",
    )
    .bind(release.id)
    .bind(duration_secs)
    .fetch_optional(&state.pool)
    .await?;

    if elapsed == Some(true) {
        // Duration complete — move to promoting for manual review
        transition_phase(state, release, "promoting", Some(100), None).await?;
        record_history(state, release, "promoted", "promoting", Some(100)).await;
    }

    Ok(())
}

/// Promoting — re-render manifests with canary as new stable, apply, finalize.
async fn handle_promoting(state: &AppState, release: &PendingRelease) -> Result<(), DeployerError> {
    let ns = target_namespace(&state.config, &release.namespace_slug, &release.environment);

    // For canary/AB: route 100% to stable (which now uses the canary image)
    if release.strategy != "rolling" {
        apply_gateway_resources(state, release, &ns, 100).await;

        // Re-render manifests with stable_image = canary_image (promotion)
        // The new stable is the canary image
        let (rendered, _sha) = render_manifests(state, release).await?;
        let _ = applier::apply_with_tracking(&state.kube, &rendered, &ns, Some(release.id)).await;
    }

    // Post-promotion: downscale the old (stable) deployment for canary strategies
    if release.strategy == "canary"
        && let Some(stable_svc) = release
            .rollout_config
            .get("stable_service")
            .and_then(|v| v.as_str())
        && let Err(e) = applier::scale(&state.kube, &ns, stable_svc, 0).await
    {
        tracing::warn!(error = %e, %stable_svc, "failed to downscale old stable deployment after promotion");
    }

    transition_phase(state, release, "completed", Some(100), Some("healthy")).await?;
    record_history(state, release, "promoted", "completed", Some(100)).await;
    fire_webhook(state, release, "deployed").await;

    let _ = crate::store::eventbus::publish(
        &state.valkey,
        &crate::store::eventbus::PlatformEvent::ReleasePromoted {
            release_id: release.id,
            project_id: release.project_id,
            image_ref: release.image_ref.clone(),
        },
    )
    .await;

    Ok(())
}

/// Rolling back — revert traffic to stable, scale canary down, mark `rolled_back`.
async fn handle_rolling_back(
    state: &AppState,
    release: &PendingRelease,
) -> Result<(), DeployerError> {
    let ns = target_namespace(&state.config, &release.namespace_slug, &release.environment);

    // Route 100% traffic to stable (0% canary)
    if release.strategy != "rolling" {
        apply_gateway_resources(state, release, &ns, 0).await;
    }

    transition_phase(state, release, "rolled_back", Some(0), Some("unhealthy")).await?;
    record_history(state, release, "rolled_back", "rolled_back", Some(0)).await;
    fire_webhook(state, release, "rolled_back").await;

    let _ = crate::store::eventbus::publish(
        &state.valkey,
        &crate::store::eventbus::PlatformEvent::ReleaseRolledBack {
            release_id: release.id,
            project_id: release.project_id,
            reason: "rollback requested".into(),
        },
    )
    .await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Gateway API helpers
// ---------------------------------------------------------------------------

/// Apply `HTTPRoute` resources for traffic splitting (references shared Gateway).
/// For canary: weighted split between `stable_service` and `canary_service`.
/// For AB test: header-based routing.
async fn apply_gateway_resources(
    state: &AppState,
    release: &PendingRelease,
    namespace: &str,
    canary_weight: i32,
) {
    let config = &release.rollout_config;
    let hostname = config
        .get("hostname")
        .and_then(|v| v.as_str())
        .unwrap_or("*");
    let route_name = format!("{}-traffic", release.project_name);
    let gw = super::gateway::GatewayRef {
        name: &state.config.gateway_name,
        namespace: &state.config.gateway_namespace,
    };

    match release.strategy.as_str() {
        "canary" => {
            let stable_svc = config
                .get("stable_service")
                .and_then(|v| v.as_str())
                .unwrap_or("stable");
            let canary_svc = config
                .get("canary_service")
                .and_then(|v| v.as_str())
                .unwrap_or("canary");

            let cw = canary_weight.unsigned_abs();
            let sw = 100 - cw;

            match super::gateway::build_weighted_httproute(
                &route_name,
                namespace,
                hostname,
                stable_svc,
                canary_svc,
                sw,
                cw,
                &gw,
            ) {
                Ok(route) => apply_json_to_k8s(state, &route, namespace).await,
                Err(e) => tracing::error!(error = %e, "failed to build weighted HTTPRoute"),
            }
        }
        "ab_test" => {
            let control_svc = config
                .get("control_service")
                .and_then(|v| v.as_str())
                .unwrap_or("control");
            let treatment_svc = config
                .get("treatment_service")
                .and_then(|v| v.as_str())
                .unwrap_or("treatment");

            let headers: std::collections::HashMap<String, String> = config
                .get("match")
                .and_then(|m| m.get("headers"))
                .and_then(|h| serde_json::from_value(h.clone()).ok())
                .unwrap_or_default();

            let route = super::gateway::build_header_match_httproute(
                &route_name,
                namespace,
                hostname,
                control_svc,
                treatment_svc,
                &headers,
                &gw,
            );
            apply_json_to_k8s(state, &route, namespace).await;
        }
        _ => {}
    }
}

/// Apply a single JSON resource to K8s via server-side apply (serialize to YAML).
async fn apply_json_to_k8s(state: &AppState, resource: &serde_json::Value, namespace: &str) {
    let yaml = match serde_yaml::to_string(resource) {
        Ok(y) => y,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize gateway resource to YAML");
            return;
        }
    };
    if let Err(e) = applier::apply_with_tracking(&state.kube, &yaml, namespace, None).await {
        tracing::warn!(error = %e, "failed to apply gateway resource");
    }
}

/// Discover the in-cluster URL for the shared Envoy Gateway proxy service.
///
/// Queries the K8s API for Services labelled with the gateway name in the gateway namespace.
/// Returns `http://{svc-name}.{namespace}.svc.cluster.local:80` or `None` if not found.
async fn resolve_gateway_url(state: &AppState) -> Option<String> {
    use kube::api::ListParams;

    let gw_ns = &state.config.gateway_namespace;
    let gw_name = &state.config.gateway_name;

    let svc_api: kube::Api<k8s_openapi::api::core::v1::Service> =
        kube::Api::namespaced(state.kube.clone(), gw_ns);

    let label = format!("gateway.envoyproxy.io/owning-gateway-name={gw_name}");
    let lp = ListParams::default().labels(&label);

    match svc_api.list(&lp).await {
        Ok(list) => {
            if let Some(svc) = list.items.first() {
                let svc_name = svc.metadata.name.as_deref()?;
                Some(format!("http://{svc_name}.{gw_ns}.svc.cluster.local:80"))
            } else {
                tracing::debug!(%gw_name, %gw_ns, "no envoy proxy service found for gateway");
                None
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to discover gateway proxy service");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Preview cleanup (merged from preview.rs)
// ---------------------------------------------------------------------------

async fn cleanup_expired_previews(state: &AppState) {
    let expired = sqlx::query(
        "SELECT dt.id, dt.project_id, dt.branch_slug, p.namespace_slug
         FROM deploy_targets dt
         JOIN projects p ON p.id = dt.project_id
         WHERE dt.environment = 'preview' AND dt.is_active = true
           AND dt.expires_at IS NOT NULL AND dt.expires_at < now()",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    for row in &expired {
        let target_id: Uuid = row.get("id");
        let project_id: Uuid = row.get("project_id");
        let branch_slug: Option<String> = row.get("branch_slug");
        let namespace_slug: String = row.get("namespace_slug");

        // Mark target inactive
        let _ = sqlx::query("UPDATE deploy_targets SET is_active = false WHERE id = $1")
            .bind(target_id)
            .execute(&state.pool)
            .await;

        // Cancel any active releases
        let _ = sqlx::query(
            "UPDATE deploy_releases SET phase = 'cancelled'
             WHERE target_id = $1 AND phase NOT IN ('completed','rolled_back','cancelled','failed')",
        )
        .bind(target_id)
        .execute(&state.pool)
        .await;

        // Delete K8s namespace
        let slug = branch_slug.as_deref().unwrap_or("unknown");
        let ns = target_namespace(&state.config, &namespace_slug, &format!("preview-{slug}"));
        if let Err(e) = crate::deployer::namespace::delete_namespace(&state.kube, &ns).await {
            tracing::warn!(error = %e, %target_id, "failed to delete preview namespace");
        }

        tracing::info!(%project_id, %target_id, "expired preview target cleaned up");
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Transition a release to a new phase, validating the state machine.
async fn transition_phase(
    state: &AppState,
    release: &PendingRelease,
    new_phase: &str,
    traffic_weight: Option<i32>,
    health: Option<&str>,
) -> Result<(), DeployerError> {
    // Validate state machine transition
    if let (Some(current), Some(next)) = (
        super::types::ReleasePhase::parse(&release.phase),
        super::types::ReleasePhase::parse(new_phase),
    ) && !current.can_transition_to(next)
    {
        tracing::warn!(
            release_id = %release.id,
            from = %release.phase,
            to = %new_phase,
            "invalid release phase transition, skipping"
        );
        return Ok(());
    }

    sqlx::query(
        "UPDATE deploy_releases SET
            phase = $2,
            traffic_weight = COALESCE($3, traffic_weight),
            health = COALESCE($4, health),
            completed_at = CASE WHEN $2 IN ('completed','rolled_back','cancelled','failed') THEN now() ELSE completed_at END
         WHERE id = $1",
    )
    .bind(release.id)
    .bind(new_phase)
    .bind(traffic_weight)
    .bind(health)
    .execute(&state.pool)
    .await?;

    Ok(())
}

/// Record a release history entry.
async fn record_history(
    state: &AppState,
    release: &PendingRelease,
    action: &str,
    phase: &str,
    traffic_weight: Option<i32>,
) {
    let _ = sqlx::query(
        "INSERT INTO release_history (release_id, target_id, action, phase, traffic_weight, image_ref, actor_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(release.id)
    .bind(release.target_id)
    .bind(action)
    .bind(phase)
    .bind(traffic_weight)
    .bind(&release.image_ref)
    .bind(release.deployed_by)
    .execute(&state.pool)
    .await;
}

/// Mark a release as failed.
pub async fn mark_failed(state: &AppState, release: &PendingRelease, message: &str) {
    let _ = sqlx::query(
        "UPDATE deploy_releases SET phase = 'failed', health = 'unhealthy', completed_at = now() WHERE id = $1",
    )
    .bind(release.id)
    .execute(&state.pool)
    .await;

    let _ = sqlx::query(
        "INSERT INTO release_history (release_id, target_id, action, phase, image_ref, detail)
         VALUES ($1, $2, 'failed', 'failed', $3, $4)",
    )
    .bind(release.id)
    .bind(release.target_id)
    .bind(&release.image_ref)
    .bind(serde_json::json!({"error": message}))
    .execute(&state.pool)
    .await;
}

/// Query deploy-scoped secrets, decrypt, inject OTEL config, create K8s Secret.
#[tracing::instrument(skip(state, release), fields(project_id = %release.project_id, %namespace))]
async fn inject_project_secrets(
    state: &AppState,
    release: &PendingRelease,
    namespace: &str,
) -> Option<String> {
    let mut env_data: BTreeMap<String, String> = BTreeMap::new();

    // Query secrets scoped to this environment (staging/prod) or 'all'.
    // Maps environment name to scope: staging→staging, production→prod.
    let env_scope = match release.environment.as_str() {
        "production" => "prod",
        other => other, // staging→staging, preview→preview
    };
    if let Some(ref master_key_str) = state.config.master_key
        && let Ok(mk) = crate::secrets::engine::parse_master_key(master_key_str)
    {
        let rows = sqlx::query(
            "SELECT name, encrypted_value FROM secrets
                 WHERE project_id = $1 AND scope IN ($3, 'all')
                   AND (environment IS NULL OR environment = $2)",
        )
        .bind(release.project_id)
        .bind(&release.environment)
        .bind(env_scope)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();

        for row in &rows {
            let name: String = row.get("name");
            let encrypted: Vec<u8> = row.get("encrypted_value");
            match crate::secrets::engine::decrypt(&encrypted, &mk, None) {
                Ok(val) => {
                    if let Ok(s) = String::from_utf8(val) {
                        env_data.insert(name, s);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to decrypt secret");
                }
            }
        }
    }

    // Inject OTEL env vars
    inject_otel_env_vars(state, release, &mut env_data).await;

    if env_data.is_empty() {
        return None;
    }

    let secret_name = format!("{namespace}-{}-secrets", env_suffix(&release.environment));
    apply_k8s_secret(
        state,
        namespace,
        &secret_name,
        &release.project_id,
        &env_data,
    )
    .await;
    Some(secret_name)
}

/// Inject OTEL environment variables into the secret data.
async fn inject_otel_env_vars(
    state: &AppState,
    release: &PendingRelease,
    env_data: &mut BTreeMap<String, String>,
) {
    env_data.insert(
        "OTEL_EXPORTER_OTLP_ENDPOINT".into(),
        state.config.platform_api_url.clone(),
    );
    env_data.insert("OTEL_SERVICE_NAME".into(), release.project_name.clone());
    env_data.insert(
        "OTEL_RESOURCE_ATTRIBUTES".into(),
        format!("platform.project_id={}", release.project_id),
    );

    // Determine scope from environment
    let scope = if release.environment == "staging" {
        "staging"
    } else {
        "prod"
    };

    if let Ok((otel_token, api_token)) =
        ensure_scoped_tokens(state, release.project_id, scope).await
    {
        env_data.insert(
            "OTEL_EXPORTER_OTLP_HEADERS".into(),
            format!("Authorization=Bearer {otel_token}"),
        );
        env_data.insert("PLATFORM_API_TOKEN".into(), api_token);
        env_data.insert(
            "PLATFORM_API_URL".into(),
            state.config.platform_api_url.clone(),
        );
        env_data.insert("PLATFORM_PROJECT_ID".into(), release.project_id.to_string());
    }
}

/// Create/rotate scoped tokens for a project deployment.
/// Returns (`otel_token`, `api_token`).
/// Scope determines the token name prefix and namespace:
///   - "agent" -> agent sessions
///   - "pipeline" -> pipeline builds
///   - "staging" -> staging environment
///   - "prod" -> production environment
pub async fn ensure_scoped_tokens(
    state: &AppState,
    project_id: Uuid,
    scope: &str,
) -> Result<(String, String), DeployerError> {
    let owner = resolve_project_owner(state, project_id).await;
    let Some((owner_id, _)) = owner else {
        return Err(DeployerError::Other(anyhow::anyhow!(
            "project owner not found"
        )));
    };

    let proj8 = &project_id.to_string()[..8];
    let otel_name = format!("otlp-{scope}-{proj8}");
    let api_name = format!("api-{scope}-{proj8}");

    // Create OTEL token (observe:write)
    let otel_token =
        ensure_single_token(state, owner_id, project_id, &otel_name, &["observe:write"]).await?;

    // Create API token (project:read — for flag evaluation)
    let api_token =
        ensure_single_token(state, owner_id, project_id, &api_name, &["project:read"]).await?;

    Ok((otel_token, api_token))
}

/// Create or rotate a single scoped API token.
async fn ensure_single_token(
    state: &AppState,
    owner_id: Uuid,
    project_id: Uuid,
    name: &str,
    scopes: &[&str],
) -> Result<String, DeployerError> {
    // Check existing valid token with matching name
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM api_tokens
         WHERE project_id = $1 AND name = $2
           AND (expires_at IS NULL OR expires_at > now())
         LIMIT 1",
    )
    .bind(project_id)
    .bind(name)
    .fetch_optional(&state.pool)
    .await?;

    let (raw_token, hash) = crate::auth::token::generate_api_token();
    let expires = chrono::Utc::now() + chrono::Duration::days(365);
    let scope_strs: Vec<String> = scopes
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let mut tx = state.pool.begin().await?;

    // Insert new token first (R3: no token gap)
    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, scopes, project_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(owner_id)
    .bind(name)
    .bind(&hash)
    .bind(&scope_strs)
    .bind(project_id)
    .bind(expires)
    .execute(&mut *tx)
    .await?;

    // Delete old token if exists
    if let Some(old_id) = existing {
        sqlx::query("DELETE FROM api_tokens WHERE id = $1")
            .bind(old_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    Ok(raw_token)
}

/// Look up the stable image from the most recent completed release for the same target.
async fn lookup_stable_image(pool: &sqlx::PgPool, target_id: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT image_ref FROM deploy_releases
         WHERE target_id = $1 AND phase = 'completed'
         ORDER BY completed_at DESC LIMIT 1",
    )
    .bind(target_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

async fn resolve_project_owner(state: &AppState, project_id: Uuid) -> Option<(Uuid, String)> {
    let owner_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()?;

    let name = sqlx::query_scalar::<_, String>("SELECT name FROM users WHERE id = $1")
        .bind(owner_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()?;

    Some((owner_id, name))
}

/// Create/refresh registry pull secret in namespace.
pub async fn ensure_registry_pull_secret_for(
    state: &AppState,
    project_id: Uuid,
    release_id: Uuid,
    namespace: &str,
) {
    let Some(registry_url) = &state.config.registry_url else {
        return;
    };

    let owner = resolve_project_owner(state, project_id).await;
    let Some((owner_id, owner_name)) = owner else {
        return;
    };

    let token = create_deploy_pull_token(state, owner_id, release_id).await;
    let Some(raw_token) = token else { return };

    let node_url = state.config.registry_node_url.as_deref();
    let docker_config = build_deploy_docker_config(registry_url, node_url, &owner_name, &raw_token);

    apply_or_replace_secret(state, namespace, REGISTRY_PULL_SECRET_NAME, &docker_config).await;
}

async fn create_deploy_pull_token(
    state: &AppState,
    owner_id: Uuid,
    release_id: Uuid,
) -> Option<String> {
    let (raw_token, hash) = crate::auth::token::generate_api_token();
    let name = format!("deploy-pull-{}", &release_id.to_string()[..8]);
    let expires = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, scopes, expires_at)
         VALUES ($1, $2, $3, ARRAY['registry:pull'], $4)",
    )
    .bind(owner_id)
    .bind(&name)
    .bind(&hash)
    .bind(expires)
    .execute(&state.pool)
    .await
    .ok()?;

    Some(raw_token)
}

fn build_deploy_docker_config(
    registry_url: &str,
    node_url: Option<&str>,
    owner_name: &str,
    raw_token: &str,
) -> serde_json::Value {
    use base64::Engine;
    let auth =
        base64::engine::general_purpose::STANDARD.encode(format!("{owner_name}:{raw_token}"));

    let mut auths = serde_json::Map::new();
    auths.insert(registry_url.to_string(), serde_json::json!({"auth": auth}));
    if let Some(node) = node_url
        && node != registry_url
    {
        auths.insert(node.to_string(), serde_json::json!({"auth": auth}));
    }
    serde_json::json!({"auths": auths})
}

async fn apply_k8s_secret(
    state: &AppState,
    namespace: &str,
    secret_name: &str,
    project_id: &Uuid,
    data: &BTreeMap<String, String>,
) {
    let secret = Secret {
        metadata: kube::api::ObjectMeta {
            name: Some(secret_name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(BTreeMap::from([(
                "platform.io/project".to_string(),
                project_id.to_string(),
            )])),
            ..Default::default()
        },
        string_data: Some(data.clone()),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    };

    let api: Api<Secret> = Api::namespaced(state.kube.clone(), namespace);
    match api.create(&PostParams::default(), &secret).await {
        Ok(_) => tracing::debug!(%secret_name, "K8s secret created"),
        Err(kube::Error::Api(resp)) if resp.code == 409 => {
            // Already exists — replace
            if let Err(e) = api
                .replace(secret_name, &PostParams::default(), &secret)
                .await
            {
                tracing::warn!(error = %e, "failed to replace K8s secret");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to create K8s secret"),
    }
}

async fn apply_or_replace_secret(
    state: &AppState,
    namespace: &str,
    secret_name: &str,
    docker_config: &serde_json::Value,
) {
    let config_str = serde_json::to_string(docker_config).unwrap_or_default();
    let mut data = BTreeMap::new();
    data.insert(
        ".dockerconfigjson".to_string(),
        k8s_openapi::ByteString(config_str.into_bytes()),
    );

    let secret = Secret {
        metadata: kube::api::ObjectMeta {
            name: Some(secret_name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("kubernetes.io/dockerconfigjson".to_string()),
        ..Default::default()
    };

    let api: Api<Secret> = Api::namespaced(state.kube.clone(), namespace);
    match api.create(&PostParams::default(), &secret).await {
        Ok(_) => tracing::debug!(%secret_name, "registry pull secret created"),
        Err(kube::Error::Api(resp)) if resp.code == 409 => {
            if let Err(e) = api
                .replace(secret_name, &PostParams::default(), &secret)
                .await
            {
                tracing::warn!(error = %e, "failed to replace registry pull secret");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to create registry pull secret"),
    }
}

/// Render manifests from ops repo or generate a basic one.
async fn render_manifests(
    state: &AppState,
    release: &PendingRelease,
) -> Result<(String, Option<String>), DeployerError> {
    if let Some(ops_repo_id) = release.ops_repo_id {
        let (repo_path, _sha, branch) = ops_repo::sync_repo(&state.pool, ops_repo_id).await?;

        let ops = sqlx::query("SELECT name, path FROM ops_repos WHERE id = $1")
            .bind(ops_repo_id)
            .fetch_one(&state.pool)
            .await?;

        let ops_path: Option<String> = ops.try_get("path").ok();
        let manifest_path = release
            .manifest_path
            .as_deref()
            .or(ops_path.as_deref())
            .unwrap_or("deploy/");
        let sha = ops_repo::get_head_sha(&repo_path).await?;

        // If manifest_path ends with '/', read all YAML files from the directory;
        // otherwise read a single file.
        let template_content = if manifest_path.ends_with('/') {
            ops_repo::read_dir_yaml_at_ref(&repo_path, &sha, manifest_path)
                .await
                .map_err(|e| DeployerError::RenderFailed(e.to_string()))?
        } else {
            ops_repo::read_file_at_ref(&repo_path, &sha, manifest_path)
                .await
                .map_err(|e| DeployerError::RenderFailed(e.to_string()))?
        };

        // Read values
        let ops_values = ops_repo::read_values(&repo_path, &branch, &release.environment)
            .await
            .unwrap_or_else(|_| serde_json::json!({}));

        let mut base_values = release
            .values_override
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        if let (Some(base), Some(ops)) = (base_values.as_object_mut(), ops_values.as_object()) {
            for (k, v) in ops {
                base.insert(k.clone(), v.clone());
            }
        }

        let image_ref = base_values
            .get("image_ref")
            .and_then(|v| v.as_str())
            .map_or_else(|| release.image_ref.clone(), String::from);

        let stable_image = Some(
            lookup_stable_image(&state.pool, release.target_id)
                .await
                .unwrap_or_else(|| image_ref.clone()),
        );
        let gateway_url = resolve_gateway_url(state).await;
        let vars = renderer::RenderVars {
            image_ref,
            project_name: release.project_name.clone(),
            environment: release.environment.clone(),
            values: base_values,
            platform_api_url: state.config.platform_api_url.clone(),
            stable_image,
            canary_image: Some(release.image_ref.clone()),
            commit_sha: release.commit_sha.clone(),
            app_image: None,
            gateway_url,
        };

        let rendered = renderer::render(&template_content, &vars)?;
        Ok((rendered, Some(sha)))
    } else {
        let manifest = generate_basic_manifest(release)?;
        Ok((manifest, None))
    }
}

/// Generate a minimal K8s Deployment manifest when no ops repo is configured.
fn generate_basic_manifest(release: &PendingRelease) -> Result<String, DeployerError> {
    // A16: Validate image_ref before interpolating into YAML to prevent injection
    crate::validation::check_container_image(&release.image_ref)
        .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;

    let name = format!("{}-{}", release.project_name, release.environment);
    Ok(format!(
        "apiVersion: apps/v1\n\
         kind: Deployment\n\
         metadata:\n\
         \x20 name: {name}\n\
         spec:\n\
         \x20 replicas: 1\n\
         \x20 selector:\n\
         \x20   matchLabels:\n\
         \x20     app: {name}\n\
         \x20 template:\n\
         \x20   metadata:\n\
         \x20     labels:\n\
         \x20       app: {name}\n\
         \x20   spec:\n\
         \x20     imagePullSecrets:\n\
         \x20     - name: {secret}\n\
         \x20     containers:\n\
         \x20     - name: app\n\
         \x20       image: {image}\n\
         \x20       ports:\n\
         \x20       - containerPort: 8080\n",
        secret = REGISTRY_PULL_SECRET_NAME,
        image = release.image_ref,
    ))
}

/// Store tracked resource inventory.
async fn store_tracked_resources(
    state: &AppState,
    release_id: Uuid,
    tracked: &[applier::TrackedResource],
) -> Result<(), DeployerError> {
    let json = serde_json::to_value(tracked)
        .map_err(|e| DeployerError::Other(anyhow::anyhow!("serialize tracked: {e}")))?;

    sqlx::query("UPDATE deploy_releases SET tracked_resources = $2 WHERE id = $1")
        .bind(release_id)
        .bind(json)
        .execute(&state.pool)
        .await?;

    Ok(())
}

async fn fire_webhook(state: &AppState, release: &PendingRelease, action: &str) {
    let payload = serde_json::json!({
        "action": action,
        "project_id": release.project_id,
        "environment": release.environment,
        "image_ref": release.image_ref,
        "release_id": release.id,
    });
    crate::api::webhooks::fire_webhooks(
        &state.pool,
        release.project_id,
        "deploy",
        &payload,
        &state.webhook_semaphore,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_release() -> PendingRelease {
        PendingRelease {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            image_ref: "registry.example.com/myapp:v1.2.3".into(),
            commit_sha: None,
            strategy: "rolling".into(),
            phase: "pending".into(),
            traffic_weight: 0,
            current_step: 0,
            rollout_config: serde_json::json!({}),
            values_override: None,
            deployed_by: Some(Uuid::new_v4()),
            pipeline_id: None,
            environment: "production".into(),
            ops_repo_id: None,
            manifest_path: None,
            branch_slug: None,
            project_name: "my-app".into(),
            namespace_slug: "my-app".into(),
            tracked_resources: Vec::new(),
            skip_prune: false,
        }
    }

    #[test]
    fn basic_manifest_has_correct_name() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("name: my-app-production"));
    }

    #[test]
    fn basic_manifest_has_correct_image() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("image: registry.example.com/myapp:v1.2.3"));
    }

    #[test]
    fn basic_manifest_is_valid_yaml() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&manifest).expect("manifest should be valid YAML");
        assert_eq!(parsed["kind"], "Deployment");
        assert_eq!(parsed["apiVersion"], "apps/v1");
        assert_eq!(parsed["spec"]["replicas"], 1);
    }

    #[test]
    fn basic_manifest_container_port_8080() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("containerPort: 8080"));
    }

    #[test]
    fn basic_manifest_selector_matches_labels() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        let selector_label = &parsed["spec"]["selector"]["matchLabels"]["app"];
        let template_label = &parsed["spec"]["template"]["metadata"]["labels"]["app"];
        assert_eq!(selector_label, template_label);
    }

    #[test]
    fn basic_manifest_different_environments() {
        for env in &["production", "staging", "development", "preview-feat-123"] {
            let mut r = sample_release();
            r.environment = (*env).to_string();
            let manifest = generate_basic_manifest(&r).unwrap();
            let expected_name = format!("name: my-app-{env}");
            assert!(
                manifest.contains(&expected_name),
                "manifest should contain '{expected_name}'"
            );
            let _: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        }
    }

    #[test]
    fn basic_manifest_different_images() {
        for image in &[
            "nginx:latest",
            "registry.io/app:v1.0.0-rc1",
            "ghcr.io/org/repo:sha-abc123",
        ] {
            let mut r = sample_release();
            r.image_ref = (*image).to_string();
            let manifest = generate_basic_manifest(&r).unwrap();
            assert!(manifest.contains(&format!("image: {image}")));
        }
    }

    #[test]
    fn basic_manifest_container_name_is_app() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        let container_name = &parsed["spec"]["template"]["spec"]["containers"][0]["name"];
        assert_eq!(container_name, "app");
    }

    #[test]
    fn basic_manifest_labels_consistent() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        let selector = &parsed["spec"]["selector"]["matchLabels"]["app"];
        let template = &parsed["spec"]["template"]["metadata"]["labels"]["app"];
        let metadata_name = &parsed["metadata"]["name"];
        assert_eq!(selector, template);
        assert_eq!(selector.as_str(), metadata_name.as_str());
    }

    #[test]
    fn target_namespace_production() {
        let config = crate::config::Config::test_default();
        assert_eq!(
            target_namespace(&config, "my-app", "production"),
            "my-app-prod"
        );
    }

    #[test]
    fn target_namespace_staging() {
        let config = crate::config::Config::test_default();
        assert_eq!(
            target_namespace(&config, "my-app", "staging"),
            "my-app-staging"
        );
    }

    #[test]
    fn target_namespace_preview() {
        let config = crate::config::Config::test_default();
        assert_eq!(
            target_namespace(&config, "my-app", "preview-feat-123"),
            "my-app-preview-feat-123"
        );
    }

    #[test]
    fn target_namespace_with_prefix() {
        let config = crate::config::Config {
            ns_prefix: Some("platform-test-abc".into()),
            ..crate::config::Config::test_default()
        };
        assert_eq!(
            target_namespace(&config, "my-app", "production"),
            "platform-test-abc-my-app-prod"
        );
    }

    #[test]
    fn build_docker_config_single_url() {
        let config =
            build_deploy_docker_config("registry.example.com:5000", None, "admin", "secret_token");
        let auths = config["auths"].as_object().unwrap();
        assert!(auths.contains_key("registry.example.com:5000"));
        assert_eq!(auths.len(), 1);
    }

    #[test]
    fn build_docker_config_with_node_url() {
        let config = build_deploy_docker_config(
            "registry.example.com:5000",
            Some("node-registry:5000"),
            "admin",
            "secret_token",
        );
        let auths = config["auths"].as_object().unwrap();
        assert!(auths.contains_key("registry.example.com:5000"));
        assert!(auths.contains_key("node-registry:5000"));
        assert_eq!(auths.len(), 2);
    }

    #[test]
    fn build_docker_config_same_urls_no_duplicate() {
        let config =
            build_deploy_docker_config("registry:5000", Some("registry:5000"), "admin", "tok");
        let auths = config["auths"].as_object().unwrap();
        assert_eq!(auths.len(), 1);
    }

    // -- env_suffix --

    #[test]
    fn env_suffix_production_to_prod() {
        assert_eq!(env_suffix("production"), "prod");
    }

    #[test]
    fn env_suffix_staging_unchanged() {
        assert_eq!(env_suffix("staging"), "staging");
    }

    #[test]
    fn env_suffix_custom_unchanged() {
        assert_eq!(env_suffix("preview-feat-123"), "preview-feat-123");
    }

    // -- injection prevention --

    #[test]
    fn basic_manifest_rejects_injection_semicolon() {
        let mut r = sample_release();
        r.image_ref = "nginx:latest; rm -rf /".into();
        let result = generate_basic_manifest(&r);
        assert!(result.is_err(), "semicolon in image_ref should be rejected");
    }

    #[test]
    fn basic_manifest_rejects_injection_backtick() {
        let mut r = sample_release();
        r.image_ref = "nginx:`whoami`".into();
        let result = generate_basic_manifest(&r);
        assert!(result.is_err(), "backtick in image_ref should be rejected");
    }

    #[test]
    fn basic_manifest_rejects_empty_image() {
        let mut r = sample_release();
        r.image_ref = String::new();
        let result = generate_basic_manifest(&r);
        assert!(result.is_err(), "empty image_ref should be rejected");
    }

    #[test]
    fn basic_manifest_includes_image_pull_secret() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(
            manifest.contains(REGISTRY_PULL_SECRET_NAME),
            "manifest should reference the registry pull secret"
        );
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        let secrets = &parsed["spec"]["template"]["spec"]["imagePullSecrets"];
        assert!(secrets.is_sequence(), "imagePullSecrets should be an array");
        assert_eq!(
            secrets[0]["name"].as_str().unwrap(),
            REGISTRY_PULL_SECRET_NAME
        );
    }

    #[test]
    fn build_docker_config_auth_is_base64() {
        let config = build_deploy_docker_config("reg:5000", None, "admin", "tok123");
        let auth_str = config["auths"]["reg:5000"]["auth"].as_str().unwrap();
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, auth_str)
            .expect("auth should be valid base64");
        let decoded_str = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_str, "admin:tok123");
    }

    #[test]
    fn env_suffix_development_unchanged() {
        assert_eq!(env_suffix("development"), "development");
    }

    #[test]
    fn target_namespace_development() {
        let config = crate::config::Config::test_default();
        assert_eq!(
            target_namespace(&config, "my-app", "development"),
            "my-app-development"
        );
    }
}
