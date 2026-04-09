// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use k8s_openapi::api::core::v1::Secret;
use kube::Api;
use kube::api::{DeleteParams, PostParams};
use sqlx::Row;
use tracing::Instrument;
use uuid::Uuid;

use crate::store::AppState;

use super::error::DeployerError;
use super::image_inspect::{self, EntrypointCache};
use super::{applier, ops_repo, renderer};

/// Global entrypoint cache (1-hour TTL per entry, used across reconciler runs).
static ENTRYPOINT_CACHE: LazyLock<EntrypointCache> = LazyLock::new(EntrypointCache::new);

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
                dt.environment, dt.ops_repo_id, dt.manifest_path, dt.branch_slug, dt.hostname as target_hostname,
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
            target_hostname: row.get("target_hostname"),
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
    pub target_hostname: Option<String>,
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
#[allow(clippy::too_many_lines)]
async fn handle_pending(state: &AppState, release: &PendingRelease) -> Result<(), DeployerError> {
    let ns = target_namespace(&state.config, &release.namespace_slug, &release.environment);

    // Ensure namespace, secrets, registry pull secret
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns,
        env_suffix(&release.environment),
        &release.project_id.to_string(),
        &state.config.platform_namespace,
        &state.config.gateway_namespace,
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

    // Inject proxy wrapper if mesh is enabled
    let rendered = if state.config.mesh_enabled {
        // Resolve entrypoints for containers missing explicit `command` fields
        // so the proxy wrapper can wrap them.
        let rendered = resolve_manifest_entrypoints(
            &rendered,
            &state.pool,
            &state.minio,
            state.config.registry_node_url.as_deref(),
        )
        .await;

        // Distroless init image: contains proxy binary + iptables, no shell
        let init_image = match state
            .config
            .registry_node_url
            .as_deref()
            .or(state.config.registry_url.as_deref())
        {
            Some(reg) => format!("{reg}/platform-proxy-init:v1"),
            None => "platform-proxy-init:v1".into(),
        };
        applier::inject_proxy_wrapper(
            &rendered,
            &applier::ProxyInjectionConfig {
                platform_api_url: state.config.platform_api_url.clone(),
                init_image,
                mesh_strict_mtls: state.config.mesh_strict_mtls,
            },
        )?
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
        tracing::info!(
            release_id = %release.id, strategy = %release.strategy,
            %initial_weight, namespace = %ns,
            "canary release started — initial traffic weight set"
        );

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
                tracing::info!(
                    release_id = %release.id, step = next_step, total_steps = steps.len(),
                    "canary all steps passed — promoting to stable"
                );
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
                tracing::info!(
                    release_id = %release.id, step = next_step, %weight,
                    "canary step advanced — traffic weight updated"
                );

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
        // Re-inject secrets (envFrom) since render_manifests produces raw templates
        let secrets_name = inject_project_secrets(state, release, &ns).await;
        let (rendered, _sha) = render_manifests(state, release).await?;
        let rendered = if let Some(ref sn) = secrets_name {
            applier::inject_env_from_secret(&rendered, sn)?
        } else {
            rendered
        };
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

    tracing::info!(
        release_id = %release.id, strategy = %release.strategy,
        image_ref = %release.image_ref,
        "canary promotion complete — 100% traffic on new stable"
    );

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
        .or(release.target_hostname.as_deref())
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

    let label = "platform.io/component=gateway".to_string();
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

/// Convert a secret key name to a K8s-safe secret name.
fn secret_name_from_key(name: &str) -> String {
    name.to_lowercase().replace('_', "-")
}

/// Create individual K8s Secrets for each user-defined project secret.
#[tracing::instrument(skip(state, release), fields(project_id = %release.project_id, %namespace))]
async fn create_user_secrets(
    state: &AppState,
    release: &PendingRelease,
    namespace: &str,
) -> Vec<String> {
    let mut created: Vec<String> = Vec::new();

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
                        let k8s_name = secret_name_from_key(&name);
                        let data = BTreeMap::from([("value".to_string(), s)]);
                        apply_k8s_secret(state, namespace, &k8s_name, &release.project_id, &data)
                            .await;
                        created.push(k8s_name);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to decrypt secret");
                }
            }
        }
    }

    created
}

/// Create the platform secret containing OTEL + platform env vars.
#[tracing::instrument(skip(state, release), fields(project_id = %release.project_id, %namespace))]
async fn inject_platform_secret(
    state: &AppState,
    release: &PendingRelease,
    namespace: &str,
) -> Option<String> {
    let mut env_data: BTreeMap<String, String> = BTreeMap::new();
    let secret_name = format!("{namespace}-{}-platform", env_suffix(&release.environment));

    inject_otel_env_vars(state, release, &mut env_data, namespace, &secret_name).await;

    if env_data.is_empty() {
        return None;
    }
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

/// Query deploy-scoped secrets, decrypt, inject OTEL config, create K8s Secrets.
///
/// Creates individual K8s Secrets per user secret, plus one platform secret
/// for OTEL/platform vars. Deletes the legacy combined secret if it exists.
/// Returns the platform secret name (for envFrom injection by the caller).
#[tracing::instrument(skip(state, release), fields(project_id = %release.project_id, %namespace))]
async fn inject_project_secrets(
    state: &AppState,
    release: &PendingRelease,
    namespace: &str,
) -> Option<String> {
    // Create individual K8s Secrets for each user secret
    let _user_secrets = create_user_secrets(state, release, namespace).await;

    // Create the platform secret (OTEL + platform vars)
    let platform_name = inject_platform_secret(state, release, namespace).await;

    // Delete the old combined secret if it exists
    let old_name = format!("{namespace}-{}-secrets", env_suffix(&release.environment));
    let api: Api<Secret> = Api::namespaced(state.kube.clone(), namespace);
    match api.delete(&old_name, &DeleteParams::default()).await {
        Ok(_) => tracing::info!(%old_name, "deleted legacy combined secret"),
        Err(kube::Error::Api(resp)) if resp.code == 404 => {
            // Not found — nothing to clean up
        }
        Err(e) => {
            tracing::warn!(error = %e, %old_name, "failed to delete legacy combined secret");
        }
    }

    platform_name
}

/// Inject OTEL environment variables into the secret data.
async fn inject_otel_env_vars(
    state: &AppState,
    release: &PendingRelease,
    env_data: &mut BTreeMap<String, String>,
    namespace: &str,
    secret_name: &str,
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
        ensure_scoped_tokens_in_ns(state, release.project_id, scope, namespace, secret_name).await
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
        env_data.insert("OTEL_API_TOKEN".into(), otel_token.clone());
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
    ensure_scoped_tokens_in_ns(state, project_id, scope, "", "").await
}

/// Like `ensure_scoped_tokens` but with namespace/secret context for token reuse.
pub async fn ensure_scoped_tokens_in_ns(
    state: &AppState,
    project_id: Uuid,
    scope: &str,
    namespace: &str,
    secret_name: &str,
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

    // Create or reuse OTEL token (observe:write)
    let otel_token = ensure_single_token(
        state,
        &TokenParams {
            owner_id,
            project_id,
            name: &otel_name,
            scopes: &["observe:write"],
            namespace,
            secret_name,
            secret_key: "OTEL_API_TOKEN",
        },
    )
    .await?;

    // Create or reuse API token (project:read)
    let api_token = ensure_single_token(
        state,
        &TokenParams {
            owner_id,
            project_id,
            name: &api_name,
            scopes: &["project:read"],
            namespace,
            secret_name,
            secret_key: "PLATFORM_API_TOKEN",
        },
    )
    .await?;

    Ok((otel_token, api_token))
}

struct TokenParams<'a> {
    owner_id: Uuid,
    project_id: Uuid,
    name: &'a str,
    scopes: &'a [&'a str],
    namespace: &'a str,
    secret_name: &'a str,
    secret_key: &'a str,
}

/// Ensure a scoped API token exists. Reuses existing valid tokens; only creates
/// a new one if none exists or the existing one is expired.
///
/// Returns the raw token string. For existing tokens, reads the raw value from
/// the K8s Secret (since we only store the hash in the DB).
async fn ensure_single_token(
    state: &AppState,
    params: &TokenParams<'_>,
) -> Result<String, DeployerError> {
    // Check existing valid token with matching name
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM api_tokens
         WHERE project_id = $1 AND name = $2
           AND (expires_at IS NULL OR expires_at > now())
         LIMIT 1",
    )
    .bind(params.project_id)
    .bind(params.name)
    .fetch_optional(&state.pool)
    .await?;

    // Reuse existing token — read raw value from the K8s Secret
    if existing.is_some() {
        if let Some(raw) = read_secret_key(
            &state.kube,
            params.namespace,
            params.secret_name,
            params.secret_key,
        )
        .await
        {
            tracing::debug!(name = params.name, "reusing existing token");
            return Ok(raw);
        }
        // Secret missing or key not found — fall through to create new token
        tracing::debug!(
            name = params.name,
            "token exists in DB but not in K8s Secret, creating new"
        );
    }

    let (raw_token, hash) = crate::auth::token::generate_api_token();
    let expires = chrono::Utc::now() + chrono::Duration::days(365);
    let scope_strs: Vec<String> = params
        .scopes
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let mut tx = state.pool.begin().await?;

    // Insert new token
    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, scopes, project_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(params.owner_id)
    .bind(params.name)
    .bind(&hash)
    .bind(&scope_strs)
    .bind(params.project_id)
    .bind(expires)
    .execute(&mut *tx)
    .await?;

    // Delete old token if exists (replaced by the new one)
    if let Some(old_id) = existing {
        sqlx::query("DELETE FROM api_tokens WHERE id = $1")
            .bind(old_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    Ok(raw_token)
}

/// Read a single key from a K8s Secret, returning the decoded string value.
async fn read_secret_key(
    kube: &kube::Client,
    namespace: &str,
    secret_name: &str,
    key: &str,
) -> Option<String> {
    let api: Api<Secret> = Api::namespaced(kube.clone(), namespace);
    let secret = api.get(secret_name).await.ok()?;
    let data = secret.data?;
    let bytes = data.get(key)?;
    String::from_utf8(bytes.0.clone()).ok()
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
// Entrypoint resolution for proxy wrapping
// ---------------------------------------------------------------------------

/// Resolve image entrypoints for containers missing an explicit `command` field.
///
/// Parses the rendered YAML, finds workload containers without `command`, resolves
/// their image entrypoint via `image_inspect`, and injects the resolved command
/// back into the manifest so `inject_proxy_wrapper` can wrap it.
async fn resolve_manifest_entrypoints(
    manifests_yaml: &str,
    pool: &sqlx::PgPool,
    minio: &opendal::Operator,
    platform_registry_url: Option<&str>,
) -> String {
    let docs = renderer::split_yaml_documents(manifests_yaml);
    let mut output_docs = Vec::with_capacity(docs.len());

    for doc_str in &docs {
        let Ok(mut doc) = serde_yaml::from_str::<serde_json::Value>(doc_str) else {
            output_docs.push(doc_str.clone());
            continue;
        };

        let kind = doc["kind"].as_str().unwrap_or_default().to_string();

        let pod_spec_path = match kind.as_str() {
            "Deployment" | "StatefulSet" | "DaemonSet" | "Job" => "/spec/template/spec",
            "CronJob" => "/spec/jobTemplate/spec/template/spec",
            _ => {
                output_docs.push(doc_str.clone());
                continue;
            }
        };

        if let Some(spec) = doc.pointer_mut(pod_spec_path)
            && let Some(containers) = spec.get_mut("containers").and_then(|v| v.as_array_mut())
        {
            for container in containers.iter_mut() {
                // Skip containers that already have a command
                let has_command = container
                    .get("command")
                    .and_then(|v| v.as_array())
                    .is_some_and(|a| !a.is_empty());
                if has_command {
                    continue;
                }

                let Some(image) = container.get("image").and_then(|v| v.as_str()) else {
                    continue;
                };

                // Resolve entrypoint from the image
                let resolved = image_inspect::resolve_entrypoint(
                    image,
                    pool,
                    minio,
                    platform_registry_url,
                    &ENTRYPOINT_CACHE,
                )
                .await;

                if let Some(ep) = resolved {
                    let full_cmd = ep.full_command();
                    if !full_cmd.is_empty() {
                        tracing::info!(
                            image = %image,
                            command = ?full_cmd,
                            "resolved image entrypoint for proxy wrapping"
                        );
                        container["command"] = serde_json::Value::Array(
                            full_cmd
                                .into_iter()
                                .map(serde_json::Value::String)
                                .collect(),
                        );
                        // Clear args — they're now part of command
                        if let Some(m) = container.as_object_mut() {
                            m.remove("args");
                        }
                    }
                } else {
                    tracing::warn!(
                        image = %image,
                        "could not resolve entrypoint — proxy wrapper will skip this container"
                    );
                }
            }
        }

        match serde_yaml::to_string(&doc) {
            Ok(yaml_str) => output_docs.push(yaml_str),
            Err(_) => output_docs.push(doc_str.clone()),
        }
    }

    output_docs.join("\n---\n")
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
            target_hostname: None,
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

    // -- generate_basic_manifest comprehensive tests --

    #[test]
    fn basic_manifest_replicas_one() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        assert_eq!(parsed["spec"]["replicas"], 1);
    }

    #[test]
    fn basic_manifest_api_version_apps_v1() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        assert_eq!(parsed["apiVersion"].as_str().unwrap(), "apps/v1");
    }

    #[test]
    fn basic_manifest_kind_deployment() {
        let r = sample_release();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        assert_eq!(parsed["kind"].as_str().unwrap(), "Deployment");
    }

    #[test]
    fn basic_manifest_metadata_name_matches_project_env() {
        let mut r = sample_release();
        r.project_name = "cool-service".into();
        r.environment = "staging".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        assert_eq!(
            parsed["metadata"]["name"].as_str().unwrap(),
            "cool-service-staging"
        );
    }

    #[test]
    fn basic_manifest_rejects_injection_dollar() {
        let mut r = sample_release();
        r.image_ref = "nginx:$(id)".into();
        let result = generate_basic_manifest(&r);
        assert!(
            result.is_err(),
            "dollar sign in image_ref should be rejected"
        );
    }

    #[test]
    fn basic_manifest_rejects_newline_injection() {
        let mut r = sample_release();
        r.image_ref = "nginx:latest\nmalicious: true".into();
        let result = generate_basic_manifest(&r);
        assert!(result.is_err(), "newline in image_ref should be rejected");
    }

    #[test]
    fn basic_manifest_with_registry_port() {
        let mut r = sample_release();
        r.image_ref = "registry.example.com:5000/myapp:v2.0".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("image: registry.example.com:5000/myapp:v2.0"));
    }

    #[test]
    fn basic_manifest_with_digest() {
        let mut r = sample_release();
        r.image_ref =
            "nginx@sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains(
            "image: nginx@sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
        ));
    }

    // -- build_deploy_docker_config edge cases --

    #[test]
    fn build_docker_config_special_chars_in_password() {
        let config = build_deploy_docker_config("reg:5000", None, "user", "p@ss:w0rd!");
        let auth_str = config["auths"]["reg:5000"]["auth"].as_str().unwrap();
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, auth_str)
            .expect("auth should be valid base64");
        let decoded_str = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_str, "user:p@ss:w0rd!");
    }

    #[test]
    fn build_docker_config_node_url_different() {
        let config = build_deploy_docker_config(
            "registry.example.com",
            Some("10.0.0.1:5000"),
            "admin",
            "tok",
        );
        let auths = config["auths"].as_object().unwrap();
        assert_eq!(auths.len(), 2);
        assert!(auths.contains_key("registry.example.com"));
        assert!(auths.contains_key("10.0.0.1:5000"));
    }

    // -- env_suffix edge cases --

    #[test]
    fn env_suffix_empty_string() {
        assert_eq!(env_suffix(""), "");
    }

    #[test]
    fn env_suffix_preview_with_branch() {
        assert_eq!(env_suffix("preview-my-feature"), "preview-my-feature");
    }

    // -- target_namespace edge cases --

    #[test]
    fn target_namespace_empty_environment() {
        let config = crate::config::Config::test_default();
        let ns = target_namespace(&config, "my-app", "");
        assert_eq!(ns, "my-app-");
    }

    #[test]
    fn target_namespace_preview_branch() {
        let config = crate::config::Config::test_default();
        let ns = target_namespace(&config, "app", "preview-feat-login");
        assert_eq!(ns, "app-preview-feat-login");
    }

    #[test]
    fn target_namespace_long_prefix() {
        let config = crate::config::Config {
            ns_prefix: Some("platform-test-very-long-prefix".into()),
            ..crate::config::Config::test_default()
        };
        assert_eq!(
            target_namespace(&config, "svc", "staging"),
            "platform-test-very-long-prefix-svc-staging"
        );
    }

    // -- sample_release construction variants --

    #[test]
    fn sample_release_default_fields() {
        let r = sample_release();
        assert_eq!(r.strategy, "rolling");
        assert_eq!(r.phase, "pending");
        assert_eq!(r.traffic_weight, 0);
        assert_eq!(r.current_step, 0);
        assert!(r.tracked_resources.is_empty());
        assert!(!r.skip_prune);
        assert!(r.pipeline_id.is_none());
        assert!(r.ops_repo_id.is_none());
        assert!(r.manifest_path.is_none());
        assert!(r.branch_slug.is_none());
        assert!(r.commit_sha.is_none());
    }

    #[test]
    fn sample_release_deployed_by_is_set() {
        let r = sample_release();
        assert!(r.deployed_by.is_some());
    }

    // -- generate_basic_manifest with special characters in project name --

    #[test]
    fn basic_manifest_with_hyphenated_project() {
        let mut r = sample_release();
        r.project_name = "my-cool-service".into();
        r.environment = "staging".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("name: my-cool-service-staging"));
    }

    #[test]
    fn basic_manifest_with_underscored_project() {
        let mut r = sample_release();
        r.project_name = "my_app".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("name: my_app-production"));
    }

    // -- build_deploy_docker_config with empty credentials --

    #[test]
    fn build_docker_config_empty_username() {
        let config = build_deploy_docker_config("reg:5000", None, "", "tok");
        let auth_str = config["auths"]["reg:5000"]["auth"].as_str().unwrap();
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, auth_str).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), ":tok");
    }

    #[test]
    fn build_docker_config_empty_token() {
        let config = build_deploy_docker_config("reg:5000", None, "admin", "");
        let auth_str = config["auths"]["reg:5000"]["auth"].as_str().unwrap();
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, auth_str).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "admin:");
    }

    // -- env_suffix with unusual inputs --

    #[test]
    fn env_suffix_case_sensitive() {
        assert_eq!(env_suffix("Production"), "Production");
        assert_eq!(env_suffix("PRODUCTION"), "PRODUCTION");
    }

    // -- target_namespace with empty slug --

    #[test]
    fn target_namespace_empty_slug() {
        let config = crate::config::Config::test_default();
        let ns = target_namespace(&config, "", "production");
        assert_eq!(ns, "-prod");
    }

    // -- REGISTRY_PULL_SECRET_NAME constant --

    #[test]
    fn registry_pull_secret_name_is_expected() {
        assert_eq!(REGISTRY_PULL_SECRET_NAME, "platform-registry-pull");
    }

    // -- generate_basic_manifest: image with SHA256 digest --

    #[test]
    fn basic_manifest_with_sha_digest() {
        let mut r = sample_release();
        r.image_ref = "registry.io/app@sha256:abc123def456".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("image: registry.io/app@sha256:abc123def456"));
    }

    // -- generate_basic_manifest: multipart registry paths --

    #[test]
    fn basic_manifest_with_nested_registry_path() {
        let mut r = sample_release();
        r.image_ref = "ghcr.io/org/team/project:v1.0.0".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("image: ghcr.io/org/team/project:v1.0.0"));
    }

    // -- build_deploy_docker_config: long registry URLs --

    #[test]
    fn build_docker_config_long_registry_url() {
        let long_url = format!("registry.{}.example.com:5000", "a".repeat(100));
        let config = build_deploy_docker_config(&long_url, None, "user", "pass");
        let auths = config["auths"].as_object().unwrap();
        assert!(auths.contains_key(&long_url));
    }

    // -- target_namespace: production env maps to prod suffix --

    #[test]
    fn target_namespace_uses_env_suffix() {
        let config = crate::config::Config::test_default();
        // Verify "production" is mapped to "prod" via env_suffix
        let ns = target_namespace(&config, "svc", "production");
        assert!(
            ns.ends_with("-prod"),
            "production should map to -prod suffix"
        );
        assert!(
            !ns.contains("production"),
            "should not contain full 'production' word"
        );
    }

    #[test]
    fn sample_release_has_expected_defaults() {
        let r = sample_release();
        assert_eq!(r.strategy, "rolling");
        assert_eq!(r.phase, "pending");
        assert_eq!(r.traffic_weight, 0);
        assert_eq!(r.current_step, 0);
        assert_eq!(r.environment, "production");
        assert!(r.ops_repo_id.is_none());
        assert!(r.manifest_path.is_none());
        assert!(r.branch_slug.is_none());
        assert!(r.pipeline_id.is_none());
        assert!(r.tracked_resources.is_empty());
        assert!(!r.skip_prune);
    }

    #[test]
    fn sample_release_canary_strategy() {
        let mut r = sample_release();
        r.strategy = "canary".into();
        r.phase = "progressing".into();
        r.traffic_weight = 20;
        r.current_step = 1;
        r.rollout_config = serde_json::json!({
            "steps": [10, 20, 50, 100],
            "stable_service": "api-stable",
            "canary_service": "api-canary",
        });
        assert_eq!(r.strategy, "canary");
        assert_eq!(r.traffic_weight, 20);
        // Verify rollout config can extract steps
        let steps = r
            .rollout_config
            .get("steps")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(steps.len(), 4);
    }

    #[test]
    fn sample_release_ab_test_strategy() {
        let mut r = sample_release();
        r.strategy = "ab_test".into();
        r.phase = "progressing".into();
        r.rollout_config = serde_json::json!({
            "duration": 86400,
            "control_service": "checkout-control",
            "treatment_service": "checkout-treatment",
            "match": { "headers": { "x-experiment": "treatment" } },
        });
        let duration = r
            .rollout_config
            .get("duration")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        assert_eq!(duration, 86400);
    }

    #[test]
    fn basic_manifest_staging_environment() {
        let mut r = sample_release();
        r.environment = "staging".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("name: my-app-staging"));
    }

    #[test]
    fn basic_manifest_preview_environment() {
        let mut r = sample_release();
        r.environment = "preview-my-feature".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("name: my-app-preview-my-feature"));
    }

    #[test]
    fn basic_manifest_different_project_name() {
        let mut r = sample_release();
        r.project_name = "api-gateway".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("name: api-gateway-production"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        assert_eq!(
            parsed["spec"]["selector"]["matchLabels"]["app"],
            "api-gateway-production"
        );
    }

    #[test]
    fn build_docker_config_auth_encoding_complex() {
        let config = build_deploy_docker_config(
            "registry:5000",
            None,
            "my-user",
            "p@$$w0rd_with_special=chars",
        );
        let auth_str = config["auths"]["registry:5000"]["auth"].as_str().unwrap();
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, auth_str)
            .expect("auth should be valid base64");
        let decoded_str = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_str, "my-user:p@$$w0rd_with_special=chars");
    }

    #[test]
    fn build_docker_config_empty_password() {
        let config = build_deploy_docker_config("registry:5000", None, "admin", "");
        let auth_str = config["auths"]["registry:5000"]["auth"].as_str().unwrap();
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, auth_str).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "admin:");
    }

    #[test]
    fn env_suffix_test_environment() {
        assert_eq!(env_suffix("test"), "test");
    }

    #[test]
    fn env_suffix_integration() {
        assert_eq!(env_suffix("integration"), "integration");
    }

    #[test]
    fn target_namespace_test_environment() {
        let config = crate::config::Config::test_default();
        assert_eq!(target_namespace(&config, "app", "test"), "app-test");
    }

    #[test]
    fn registry_pull_secret_name_is_fixed() {
        assert_eq!(REGISTRY_PULL_SECRET_NAME, "platform-registry-pull");
    }

    #[test]
    fn tracked_resources_empty_vec_serializes() {
        let tracked: Vec<applier::TrackedResource> = Vec::new();
        let json = serde_json::to_value(&tracked).unwrap();
        let parsed: Vec<applier::TrackedResource> = serde_json::from_value(json).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn tracked_resources_from_invalid_json() {
        let bad_json = serde_json::json!("not an array");
        let result = serde_json::from_value::<Vec<applier::TrackedResource>>(bad_json);
        assert!(result.is_err());
    }

    #[test]
    fn tracked_resources_from_null_json() {
        let null_json = serde_json::json!(null);
        let result = serde_json::from_value::<Vec<applier::TrackedResource>>(null_json);
        assert!(result.is_err());
    }

    #[test]
    fn tracked_resources_from_object_json() {
        let obj_json = serde_json::json!({"key": "value"});
        let result = serde_json::from_value::<Vec<applier::TrackedResource>>(obj_json);
        assert!(result.is_err());
    }

    // -- secret_name_from_key --

    #[test]
    fn secret_name_from_key_basic() {
        assert_eq!(secret_name_from_key("DATABASE_URL"), "database-url");
    }

    #[test]
    fn secret_name_from_key_single_word() {
        assert_eq!(secret_name_from_key("HOSTNAME"), "hostname");
    }

    #[test]
    fn secret_name_from_key_multi_underscore() {
        assert_eq!(secret_name_from_key("MY_DB_URL"), "my-db-url");
    }

    #[test]
    fn secret_name_from_key_already_lowercase() {
        assert_eq!(secret_name_from_key("my-secret"), "my-secret");
    }

    #[test]
    fn basic_manifest_image_with_sha256_digest() {
        let mut r = sample_release();
        r.image_ref = "myregistry.io/app@sha256:aabbccddee1122334455667788990011aabbccddee1122334455667788990011".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("image: myregistry.io/app@sha256:aabbccddee1122334455667788990011aabbccddee1122334455667788990011"));
    }

    #[test]
    fn basic_manifest_with_localhost_registry() {
        let mut r = sample_release();
        r.image_ref = "localhost:5000/myapp:v1".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("image: localhost:5000/myapp:v1"));
    }

    #[test]
    fn basic_manifest_hyphenated_project_name() {
        let mut r = sample_release();
        r.project_name = "my-complex-app-name".into();
        r.environment = "staging".into();
        let manifest = generate_basic_manifest(&r).unwrap();
        assert!(manifest.contains("name: my-complex-app-name-staging"));
    }
}
