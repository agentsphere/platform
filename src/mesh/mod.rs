//! Service mesh CA module.
//!
//! Provides a SPIFFE-based certificate authority for mTLS between services.
//! The root CA key is encrypted at rest using the platform secrets engine.

#[allow(dead_code)]
pub mod acme;
pub mod ca;
pub mod error;
pub mod identity;

pub use ca::MeshCa;
#[allow(unused_imports)]
pub use identity::SpiffeId;

use std::time::Duration;

use tracing::Instrument;

use crate::store::AppState;

/// Background task: periodically sync trust bundle `ConfigMap` to all managed namespaces.
///
/// Runs every 5 minutes to cover newly created namespaces. If the mesh CA is
/// not enabled (`state.mesh_ca` is `None`), the task returns immediately.
pub async fn sync_trust_bundles(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    let Some(ref mesh_ca) = state.mesh_ca else {
        return;
    };

    let ca_pem = mesh_ca.trust_bundle().to_owned();
    let mut interval = tokio::time::interval(Duration::from_secs(300));

    state.task_registry.register("mesh_trust_bundle_sync", 10);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                let span = tracing::info_span!(
                    "task_iteration",
                    task_name = "mesh_trust_bundle_sync",
                    trace_id = %iter_trace_id,
                    source = "system",
                );
                async {
                    match sync_bundles_to_namespaces(&state.kube, &ca_pem).await {
                        Ok(count) => {
                            state.task_registry.heartbeat("mesh_trust_bundle_sync");
                            tracing::debug!(namespaces = count, "trust bundle sync complete");
                        }
                        Err(e) => {
                            state.task_registry.report_error(
                                "mesh_trust_bundle_sync",
                                &e.to_string(),
                            );
                            tracing::warn!(error = %e, "trust bundle sync failed");
                        }
                    }
                }
                .instrument(span)
                .await;
            }
            _ = shutdown.changed() => {
                tracing::info!("mesh trust bundle sync shutting down");
                break;
            }
        }
    }
}

/// List all namespaces with `platform.io/managed-by=platform` and ensure
/// each has an up-to-date `mesh-ca-bundle` `ConfigMap`. Returns the count
/// of namespaces synced.
#[tracing::instrument(skip(kube_client, ca_pem), err)]
async fn sync_bundles_to_namespaces(
    kube_client: &kube::Client,
    ca_pem: &str,
) -> Result<usize, anyhow::Error> {
    use kube::Api;
    use kube::api::ListParams;

    let ns_api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(kube_client.clone());
    let lp = ListParams::default().labels("platform.io/managed-by=platform");
    let ns_list = ns_api.list(&lp).await?;

    let mut count = 0;
    for ns in &ns_list.items {
        let ns_name = ns.metadata.name.as_deref().unwrap_or("");
        if ns_name.is_empty() {
            continue;
        }
        if let Err(e) =
            crate::deployer::namespace::ensure_mesh_ca_bundle(kube_client, ns_name, ca_pem).await
        {
            tracing::warn!(namespace = %ns_name, error = %e, "failed to sync trust bundle");
        } else {
            count += 1;
        }
    }

    Ok(count)
}
