use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use kube::Api;
use kube::api::{DynamicObject, Patch, PatchParams};
use kube::discovery::ApiResource;

use super::error::DeployerError;
use super::renderer;

/// A successfully applied resource.
#[derive(Debug)]
pub struct AppliedResource {
    pub kind: String,
    pub name: String,
}

/// Apply rendered YAML manifests to the cluster using server-side apply.
#[tracing::instrument(skip(kube_client, manifests_yaml), fields(%namespace), err)]
pub async fn apply(
    kube_client: &kube::Client,
    manifests_yaml: &str,
    namespace: &str,
) -> Result<Vec<AppliedResource>, DeployerError> {
    let docs = renderer::split_yaml_documents(manifests_yaml);
    let mut applied = Vec::new();

    for doc_str in &docs {
        let doc: serde_json::Value = serde_yaml::from_str(doc_str)
            .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;

        let (ar, obj) = api_resource_from_yaml(&doc)?;
        let name = obj
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| DeployerError::InvalidManifest("missing metadata.name".into()))?
            .to_owned();

        // Use per-resource namespace if specified, otherwise fall back to deployment namespace
        let ns = obj.metadata.namespace.as_deref().unwrap_or(namespace);

        let api: Api<DynamicObject> = Api::namespaced_with(kube_client.clone(), ns, &ar);

        let patch_params = PatchParams::apply("platform-deployer").force();
        api.patch(&name, &patch_params, &Patch::Apply(&obj)).await?;

        tracing::info!(kind = %ar.kind, %name, %ns, "resource applied");
        applied.push(AppliedResource {
            kind: ar.kind.clone(),
            name,
        });
    }

    Ok(applied)
}

/// Wait for a Deployment to become healthy (Available=True).
/// Returns Ok(true) if healthy, Err(HealthTimeout) if timeout exceeded.
#[tracing::instrument(skip(kube_client), fields(%namespace, %deployment_name), err)]
pub async fn wait_healthy(
    kube_client: &kube::Client,
    namespace: &str,
    deployment_name: &str,
    timeout: Duration,
) -> Result<bool, DeployerError> {
    let deployments: Api<Deployment> = Api::namespaced(kube_client.clone(), namespace);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err(DeployerError::HealthTimeout(timeout.as_secs()));
        }

        tokio::time::sleep(Duration::from_secs(5)).await;

        let deploy = match deployments.get(deployment_name).await {
            Ok(d) => d,
            Err(kube::Error::Api(err)) if err.code == 404 => continue,
            Err(e) => return Err(e.into()),
        };

        if let Some(status) = &deploy.status {
            let available = status
                .conditions
                .as_ref()
                .and_then(|conds| {
                    conds
                        .iter()
                        .find(|c| c.type_ == "Available" && c.status == "True")
                })
                .is_some();

            if available {
                tracing::info!(%deployment_name, "deployment healthy");
                return Ok(true);
            }
        }
    }
}

/// Scale a Deployment to the given number of replicas.
#[tracing::instrument(skip(kube_client), fields(%namespace, %deployment_name, %replicas), err)]
pub async fn scale(
    kube_client: &kube::Client,
    namespace: &str,
    deployment_name: &str,
    replicas: i32,
) -> Result<(), DeployerError> {
    let deployments: Api<Deployment> = Api::namespaced(kube_client.clone(), namespace);

    let patch = serde_json::json!({
        "spec": {
            "replicas": replicas
        }
    });

    deployments
        .patch(
            deployment_name,
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await?;

    tracing::info!(%deployment_name, %replicas, "deployment scaled");
    Ok(())
}

/// Find the first Deployment resource name from a list of applied resources.
pub fn find_deployment_name(applied: &[AppliedResource]) -> Option<&str> {
    applied
        .iter()
        .find(|r| r.kind == "Deployment")
        .map(|r| r.name.as_str())
}

// ---------------------------------------------------------------------------
// YAML → kube-rs DynamicObject helpers
// ---------------------------------------------------------------------------

fn api_resource_from_yaml(
    doc: &serde_json::Value,
) -> Result<(ApiResource, DynamicObject), DeployerError> {
    let api_version = doc["apiVersion"]
        .as_str()
        .ok_or_else(|| DeployerError::InvalidManifest("missing apiVersion".into()))?;
    let kind = doc["kind"]
        .as_str()
        .ok_or_else(|| DeployerError::InvalidManifest("missing kind".into()))?;

    let (group, version) = parse_api_version(api_version);
    let plural = kind_to_plural(kind);

    let ar = ApiResource {
        group: group.to_owned(),
        version: version.to_owned(),
        api_version: api_version.to_owned(),
        kind: kind.to_owned(),
        plural: plural.clone(),
    };

    let obj: DynamicObject = serde_json::from_value(doc.clone())
        .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;

    Ok((ar, obj))
}

/// Parse "apps/v1" → ("apps", "v1"), "v1" → ("", "v1")
fn parse_api_version(api_version: &str) -> (&str, &str) {
    match api_version.rsplit_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    }
}

/// Map a K8s kind to its plural resource name.
fn kind_to_plural(kind: &str) -> String {
    match kind {
        "Deployment" => "deployments".into(),
        "Service" => "services".into(),
        "ConfigMap" => "configmaps".into(),
        "Secret" => "secrets".into(),
        "Ingress" => "ingresses".into(),
        "ServiceAccount" => "serviceaccounts".into(),
        "HorizontalPodAutoscaler" => "horizontalpodautoscalers".into(),
        "PodDisruptionBudget" => "poddisruptionbudgets".into(),
        "Namespace" => "namespaces".into(),
        "Role" => "roles".into(),
        "RoleBinding" => "rolebindings".into(),
        "ClusterRole" => "clusterroles".into(),
        "ClusterRoleBinding" => "clusterrolebindings".into(),
        "Job" => "jobs".into(),
        "CronJob" => "cronjobs".into(),
        "StatefulSet" => "statefulsets".into(),
        "DaemonSet" => "daemonsets".into(),
        "PersistentVolumeClaim" => "persistentvolumeclaims".into(),
        // Fallback: lowercase + "s" (works for most standard resources)
        other => format!("{}s", other.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_core_api_version() {
        let (group, version) = parse_api_version("v1");
        assert_eq!(group, "");
        assert_eq!(version, "v1");
    }

    #[test]
    fn parse_group_api_version() {
        let (group, version) = parse_api_version("apps/v1");
        assert_eq!(group, "apps");
        assert_eq!(version, "v1");
    }

    #[test]
    fn parse_networking_api_version() {
        let (group, version) = parse_api_version("networking.k8s.io/v1");
        assert_eq!(group, "networking.k8s.io");
        assert_eq!(version, "v1");
    }

    #[test]
    fn known_kinds_to_plural() {
        assert_eq!(kind_to_plural("Deployment"), "deployments");
        assert_eq!(kind_to_plural("Service"), "services");
        assert_eq!(kind_to_plural("ConfigMap"), "configmaps");
        assert_eq!(kind_to_plural("Ingress"), "ingresses");
        assert_eq!(
            kind_to_plural("HorizontalPodAutoscaler"),
            "horizontalpodautoscalers"
        );
    }

    #[test]
    fn unknown_kind_fallback() {
        assert_eq!(kind_to_plural("Widget"), "widgets");
        assert_eq!(kind_to_plural("MyCustomResource"), "mycustomresources");
    }

    #[test]
    fn api_resource_from_deployment_yaml() {
        let doc = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "test-deploy", "namespace": "default"},
            "spec": {}
        });

        let (ar, obj) = api_resource_from_yaml(&doc).unwrap();
        assert_eq!(ar.group, "apps");
        assert_eq!(ar.version, "v1");
        assert_eq!(ar.kind, "Deployment");
        assert_eq!(ar.plural, "deployments");
        assert_eq!(obj.metadata.name.as_deref(), Some("test-deploy"));
    }

    #[test]
    fn api_resource_from_service_yaml() {
        let doc = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": "test-svc"},
            "spec": {}
        });

        let (ar, _obj) = api_resource_from_yaml(&doc).unwrap();
        assert_eq!(ar.group, "");
        assert_eq!(ar.version, "v1");
        assert_eq!(ar.kind, "Service");
    }

    #[test]
    fn api_resource_missing_kind_errors() {
        let doc = serde_json::json!({
            "apiVersion": "v1",
            "metadata": {"name": "test"}
        });

        assert!(api_resource_from_yaml(&doc).is_err());
    }

    #[test]
    fn api_resource_missing_api_version_errors() {
        let doc = serde_json::json!({
            "kind": "Service",
            "metadata": {"name": "test"}
        });

        assert!(api_resource_from_yaml(&doc).is_err());
    }

    #[test]
    fn find_deployment_name_works() {
        let applied = vec![
            AppliedResource {
                kind: "ConfigMap".into(),
                name: "cfg".into(),
            },
            AppliedResource {
                kind: "Deployment".into(),
                name: "my-deploy".into(),
            },
            AppliedResource {
                kind: "Service".into(),
                name: "svc".into(),
            },
        ];
        assert_eq!(find_deployment_name(&applied), Some("my-deploy"));
    }

    #[test]
    fn find_deployment_name_none() {
        let applied = vec![AppliedResource {
            kind: "ConfigMap".into(),
            name: "cfg".into(),
        }];
        assert_eq!(find_deployment_name(&applied), None);
    }
}
