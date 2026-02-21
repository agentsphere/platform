use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, Namespace, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Patch, PatchParams};
use kube::Api;
use uuid::Uuid;

use crate::store::AppState;

// ---------------------------------------------------------------------------
// Background reconciliation loop
// ---------------------------------------------------------------------------

/// Background task: reconcile preview deployments every 15 seconds.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("preview reconciler started");

    let mut interval = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = reconcile(&state).await {
                    tracing::error!(error = %e, "preview reconciliation failed");
                }
                if let Err(e) = cleanup_expired(&state).await {
                    tracing::error!(error = %e, "preview cleanup failed");
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("preview reconciler shutting down");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reconcile pending previews
// ---------------------------------------------------------------------------

struct PendingPreview {
    id: Uuid,
    project_id: Uuid,
    branch_slug: String,
    image_ref: String,
    project_name: String,
}

/// Find previews needing reconciliation and spawn tasks for each.
async fn reconcile(state: &AppState) -> Result<(), anyhow::Error> {
    let pending = sqlx::query!(
        r#"SELECT pd.id, pd.project_id, pd.branch_slug, pd.image_ref,
                  p.name as "project_name!: String"
           FROM preview_deployments pd
           JOIN projects p ON p.id = pd.project_id AND p.is_active = true
           WHERE pd.desired_status = 'active'
             AND pd.current_status IN ('pending', 'syncing')
           LIMIT 5"#,
    )
    .fetch_all(&state.pool)
    .await?;

    for row in pending {
        let preview = PendingPreview {
            id: row.id,
            project_id: row.project_id,
            branch_slug: row.branch_slug,
            image_ref: row.image_ref,
            project_name: row.project_name,
        };
        let state = state.clone();
        tokio::spawn(reconcile_one(state, preview));
    }

    Ok(())
}

/// Reconcile a single preview deployment: mark syncing, apply K8s manifests,
/// then update status to healthy or failed.
async fn reconcile_one(state: AppState, preview: PendingPreview) {
    // Mark as syncing
    let _ = sqlx::query!(
        "UPDATE preview_deployments SET current_status = 'syncing', updated_at = now() WHERE id = $1",
        preview.id,
    )
    .execute(&state.pool)
    .await;

    match apply_preview_manifests(&state, &preview).await {
        Ok(()) => {
            let _ = sqlx::query!(
                "UPDATE preview_deployments SET current_status = 'healthy', updated_at = now() WHERE id = $1",
                preview.id,
            )
            .execute(&state.pool)
            .await;
            tracing::info!(
                preview_id = %preview.id,
                slug = %preview.branch_slug,
                "preview deployed successfully"
            );
        }
        Err(e) => {
            let _ = sqlx::query!(
                "UPDATE preview_deployments SET current_status = 'failed', updated_at = now() WHERE id = $1",
                preview.id,
            )
            .execute(&state.pool)
            .await;
            tracing::error!(
                preview_id = %preview.id,
                error = %e,
                "preview deployment failed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// K8s manifest application
// ---------------------------------------------------------------------------

/// Apply K8s namespace, Deployment, and Service for a preview.
#[tracing::instrument(skip(state), fields(preview_id = %preview.id, slug = %preview.branch_slug), err)]
async fn apply_preview_manifests(
    state: &AppState,
    preview: &PendingPreview,
) -> Result<(), anyhow::Error> {
    let project_slug = crate::pipeline::slug(&preview.project_name);
    let ns_name = build_namespace_name(&project_slug, &preview.branch_slug);

    // Ensure namespace exists
    ensure_namespace(&state.kube, &ns_name).await?;

    // Create/update Deployment
    let deployment = build_preview_deployment(preview, &ns_name);
    apply_deployment(&state.kube, &ns_name, &deployment).await?;

    // Create/update Service
    let service = build_preview_service(preview, &ns_name);
    apply_service(&state.kube, &ns_name, &service).await?;

    Ok(())
}

/// Build the K8s namespace name for a preview, respecting the 63-char DNS label limit.
fn build_namespace_name(project_slug: &str, branch_slug: &str) -> String {
    let raw = format!("preview-{project_slug}-{branch_slug}");
    if raw.len() > 63 {
        raw[..63].trim_end_matches('-').to_string()
    } else {
        raw
    }
}

/// Ensure a K8s namespace exists, creating it if necessary.
async fn ensure_namespace(kube: &kube::Client, ns_name: &str) -> Result<(), anyhow::Error> {
    let namespaces: Api<Namespace> = Api::all(kube.clone());

    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(ns_name.to_string()),
            labels: Some(BTreeMap::from([
                ("platform.io/component".into(), "preview".into()),
                ("platform.io/managed-by".into(), "platform".into()),
            ])),
            ..Default::default()
        },
        ..Default::default()
    };

    // Use server-side apply to be idempotent
    let patch_params = PatchParams::apply("platform-preview").force();
    namespaces
        .patch(ns_name, &patch_params, &Patch::Apply(&ns))
        .await?;

    tracing::debug!(namespace = %ns_name, "namespace ensured");
    Ok(())
}

/// Build a K8s Deployment for a preview: single replica, port 8080, with resource limits.
fn build_preview_deployment(preview: &PendingPreview, namespace: &str) -> Deployment {
    let labels = BTreeMap::from([
        ("platform.io/component".into(), "preview".into()),
        ("platform.io/project".into(), preview.project_id.to_string()),
        (
            "platform.io/branch-slug".into(),
            preview.branch_slug.clone(),
        ),
        ("app".into(), "preview".into()),
    ]);

    Deployment {
        metadata: ObjectMeta {
            name: Some("preview".into()),
            namespace: Some(namespace.into()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                match_labels: Some(BTreeMap::from([("app".into(), "preview".into())])),
                ..Default::default()
            },
            template: k8s_openapi::api::core::v1::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(k8s_openapi::api::core::v1::PodSpec {
                    containers: vec![Container {
                        name: "app".into(),
                        image: Some(preview.image_ref.clone()),
                        ports: Some(vec![ContainerPort {
                            container_port: 8080,
                            ..Default::default()
                        }]),
                        resources: Some(k8s_openapi::api::core::v1::ResourceRequirements {
                            requests: Some(BTreeMap::from([
                                ("cpu".into(), Quantity("100m".into())),
                                ("memory".into(), Quantity("128Mi".into())),
                            ])),
                            limits: Some(BTreeMap::from([
                                ("cpu".into(), Quantity("500m".into())),
                                ("memory".into(), Quantity("512Mi".into())),
                            ])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a K8s ClusterIP Service for a preview, targeting port 8080.
fn build_preview_service(preview: &PendingPreview, namespace: &str) -> Service {
    let labels = BTreeMap::from([
        ("platform.io/component".into(), "preview".into()),
        ("platform.io/project".into(), preview.project_id.to_string()),
        (
            "platform.io/branch-slug".into(),
            preview.branch_slug.clone(),
        ),
    ]);

    Service {
        metadata: ObjectMeta {
            name: Some("preview".into()),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([("app".into(), "preview".into())])),
            ports: Some(vec![ServicePort {
                port: 80,
                target_port: Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                    8080,
                )),
                protocol: Some("TCP".into()),
                ..Default::default()
            }]),
            type_: Some("ClusterIP".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Apply a Deployment using server-side apply.
async fn apply_deployment(
    kube: &kube::Client,
    namespace: &str,
    deployment: &Deployment,
) -> Result<(), anyhow::Error> {
    let deployments: Api<Deployment> = Api::namespaced(kube.clone(), namespace);
    let patch_params = PatchParams::apply("platform-preview").force();
    deployments
        .patch("preview", &patch_params, &Patch::Apply(deployment))
        .await?;
    tracing::debug!(%namespace, "preview deployment applied");
    Ok(())
}

/// Apply a Service using server-side apply.
async fn apply_service(
    kube: &kube::Client,
    namespace: &str,
    service: &Service,
) -> Result<(), anyhow::Error> {
    let services: Api<Service> = Api::namespaced(kube.clone(), namespace);
    let patch_params = PatchParams::apply("platform-preview").force();
    services
        .patch("preview", &patch_params, &Patch::Apply(service))
        .await?;
    tracing::debug!(%namespace, "preview service applied");
    Ok(())
}

// ---------------------------------------------------------------------------
// Cleanup expired previews
// ---------------------------------------------------------------------------

/// Find expired preview deployments, mark them as stopped, and delete their K8s namespaces.
async fn cleanup_expired(state: &AppState) -> Result<(), anyhow::Error> {
    let expired = sqlx::query!(
        r#"SELECT id, project_id, branch_slug
           FROM preview_deployments
           WHERE desired_status = 'active'
             AND expires_at < now()"#,
    )
    .fetch_all(&state.pool)
    .await?;

    for row in expired {
        tracing::info!(
            preview_id = %row.id,
            slug = %row.branch_slug,
            "cleaning up expired preview"
        );

        // Mark as stopped
        let _ = sqlx::query!(
            "UPDATE preview_deployments SET desired_status = 'stopped', current_status = 'stopped', updated_at = now() WHERE id = $1",
            row.id,
        )
        .execute(&state.pool)
        .await;

        // Delete K8s namespace (cascading delete cleans up all resources)
        let project_name = sqlx::query_scalar!(
            "SELECT name FROM projects WHERE id = $1",
            row.project_id,
        )
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or_default();

        let project_slug = crate::pipeline::slug(&project_name);
        let ns_name = build_namespace_name(&project_slug, &row.branch_slug);

        if let Err(e) = delete_namespace(&state.kube, &ns_name).await {
            tracing::warn!(error = %e, namespace = %ns_name, "failed to delete preview namespace");
        }
    }

    Ok(())
}

/// Delete a K8s namespace. Ignores 404 (already deleted).
async fn delete_namespace(kube: &kube::Client, ns_name: &str) -> Result<(), anyhow::Error> {
    let namespaces: Api<Namespace> = Api::all(kube.clone());
    match namespaces
        .delete(ns_name, &kube::api::DeleteParams::default())
        .await
    {
        Ok(_) => {
            tracing::info!(namespace = %ns_name, "preview namespace deleted");
            Ok(())
        }
        Err(kube::Error::Api(err)) if err.code == 404 => {
            tracing::debug!(namespace = %ns_name, "preview namespace already deleted");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Stop a preview deployment for a given project and branch slug.
/// Called when an MR is merged to clean up the preview for the source branch.
pub async fn stop_preview_for_branch(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    branch: &str,
) {
    let slug = crate::pipeline::slugify_branch(branch);

    let _ = sqlx::query!(
        r#"UPDATE preview_deployments
           SET desired_status = 'stopped', updated_at = now()
           WHERE project_id = $1 AND branch_slug = $2 AND desired_status = 'active'"#,
        project_id,
        slug,
    )
    .execute(pool)
    .await;

    tracing::info!(
        %project_id,
        branch = %branch,
        branch_slug = %slug,
        "preview deployment stopped"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_name_basic() {
        let ns = build_namespace_name("my-project", "feature-login");
        assert_eq!(ns, "preview-my-project-feature-login");
    }

    #[test]
    fn namespace_name_truncates_to_63() {
        let ns = build_namespace_name(&"a".repeat(30), &"b".repeat(30));
        assert!(ns.len() <= 63);
        assert!(!ns.ends_with('-'));
    }

    #[test]
    fn namespace_name_short_enough() {
        let ns = build_namespace_name("proj", "br");
        assert_eq!(ns, "preview-proj-br");
    }

    #[test]
    fn build_deployment_has_correct_structure() {
        let preview = PendingPreview {
            id: Uuid::nil(),
            project_id: Uuid::nil(),
            branch_slug: "feature-test".into(),
            image_ref: "registry.example.com/app:abc123".into(),
            project_name: "test-project".into(),
        };

        let deployment = build_preview_deployment(&preview, "preview-test-project-feature-test");

        assert_eq!(deployment.metadata.name.as_deref(), Some("preview"));
        assert_eq!(
            deployment.metadata.namespace.as_deref(),
            Some("preview-test-project-feature-test")
        );

        let spec = deployment.spec.as_ref().expect("spec should be set");
        assert_eq!(spec.replicas, Some(1));

        let containers = &spec.template.spec.as_ref().expect("pod spec").containers;
        assert_eq!(containers.len(), 1);
        assert_eq!(
            containers[0].image.as_deref(),
            Some("registry.example.com/app:abc123")
        );

        let limits = containers[0]
            .resources
            .as_ref()
            .expect("resources")
            .limits
            .as_ref()
            .expect("limits");
        assert_eq!(limits["cpu"], Quantity("500m".into()));
        assert_eq!(limits["memory"], Quantity("512Mi".into()));
    }

    #[test]
    fn build_service_has_correct_structure() {
        let preview = PendingPreview {
            id: Uuid::nil(),
            project_id: Uuid::nil(),
            branch_slug: "feature-test".into(),
            image_ref: "registry.example.com/app:abc123".into(),
            project_name: "test-project".into(),
        };

        let service = build_preview_service(&preview, "preview-test-project-feature-test");

        assert_eq!(service.metadata.name.as_deref(), Some("preview"));
        assert_eq!(
            service.metadata.namespace.as_deref(),
            Some("preview-test-project-feature-test")
        );

        let spec = service.spec.as_ref().expect("spec should be set");
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));

        let ports = spec.ports.as_ref().expect("ports");
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 80);
    }
}
