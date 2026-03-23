use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use kube::Api;
use kube::api::{DeleteParams, DynamicObject, Patch, PatchParams};
use kube::discovery::ApiResource;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::DeployerError;
use super::renderer;

/// A successfully applied resource.
#[derive(Debug)]
pub struct AppliedResource {
    pub kind: String,
    pub name: String,
}

/// A tracked K8s resource for inventory-based cascade deletes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedResource {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

/// Namespaced resource kinds allowed in user-controlled deploy/ manifests.
/// Cluster-scoped types (`ClusterRole`, `ClusterRoleBinding`, `Namespace`) are
/// rejected to prevent privilege escalation and tenant isolation bypass.
const ALLOWED_KINDS: &[&str] = &[
    "Deployment",
    "Service",
    "ConfigMap",
    "Secret",
    "Ingress",
    "ServiceAccount",
    "HorizontalPodAutoscaler",
    "PodDisruptionBudget",
    "Role",
    "RoleBinding",
    "Job",
    "CronJob",
    "StatefulSet",
    "DaemonSet",
    "PersistentVolumeClaim",
    "NetworkPolicy",
    // Gateway API (Envoy Gateway)
    "HTTPRoute",
    "Gateway",
];

/// Apply manifests with optional deployment tracking.
/// When `deployment_id` is provided, injects managed-by labels on each resource.
///
/// Security: all resources are forced into the target `namespace` regardless
/// of any `metadata.namespace` in the YAML. Cluster-scoped kinds are rejected.
#[tracing::instrument(skip(kube_client, manifests_yaml), fields(%namespace), err)]
pub async fn apply_with_tracking(
    kube_client: &kube::Client,
    manifests_yaml: &str,
    namespace: &str,
    deployment_id: Option<Uuid>,
) -> Result<Vec<AppliedResource>, DeployerError> {
    let docs = renderer::split_yaml_documents(manifests_yaml);
    let mut applied = Vec::new();

    for doc_str in &docs {
        let mut doc: serde_json::Value = serde_yaml::from_str(doc_str)
            .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;

        // Inject managed-by labels when tracking is enabled
        if let Some(did) = deployment_id {
            inject_managed_labels(&mut doc, did);
        }

        let (ar, obj) = api_resource_from_yaml(&doc)?;

        // R2: Reject cluster-scoped resource types
        if !ALLOWED_KINDS.contains(&ar.kind.as_str()) {
            return Err(DeployerError::InvalidManifest(format!(
                "resource kind '{}' is not allowed in deploy manifests",
                ar.kind
            )));
        }

        let name = obj
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| DeployerError::InvalidManifest("missing metadata.name".into()))?
            .to_owned();

        // R1: Always use the deployment namespace — ignore per-resource namespace
        // to prevent cross-tenant resource injection.
        if let Some(res_ns) = obj.metadata.namespace.as_deref()
            && res_ns != namespace
        {
            tracing::warn!(
                kind = %ar.kind,
                %name,
                manifest_namespace = res_ns,
                enforced_namespace = namespace,
                "manifest namespace overridden to deployment namespace"
            );
        }

        let api: Api<DynamicObject> = Api::namespaced_with(kube_client.clone(), namespace, &ar);

        let patch_params = PatchParams::apply("platform-deployer").force();
        api.patch(&name, &patch_params, &Patch::Apply(&obj)).await?;

        tracing::info!(kind = %ar.kind, %name, namespace, "resource applied");
        applied.push(AppliedResource {
            kind: ar.kind.clone(),
            name,
        });
    }

    Ok(applied)
}

/// Build a `TrackedResource` inventory from applied manifests.
pub fn build_tracked_inventory(manifests_yaml: &str, namespace: &str) -> Vec<TrackedResource> {
    let docs = renderer::split_yaml_documents(manifests_yaml);
    let mut tracked = Vec::new();

    for doc_str in &docs {
        let doc: serde_json::Value = match serde_yaml::from_str(doc_str) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "skipping invalid YAML document in inventory");
                continue;
            }
        };

        let api_version = doc["apiVersion"].as_str().unwrap_or_default().to_owned();
        let kind = doc["kind"].as_str().unwrap_or_default().to_owned();
        let name = doc["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let ns = doc["metadata"]["namespace"]
            .as_str()
            .unwrap_or(namespace)
            .to_owned();

        if !kind.is_empty() && !name.is_empty() {
            tracked.push(TrackedResource {
                api_version,
                kind,
                name,
                namespace: ns,
            });
        }
    }

    tracked
}

/// Find orphaned resources: resources in `old` that are not in `new`.
pub fn find_orphans(old: &[TrackedResource], new: &[TrackedResource]) -> Vec<TrackedResource> {
    old.iter().filter(|o| !new.contains(o)).cloned().collect()
}

/// Delete orphaned resources from the cluster.
/// Skips resources annotated with `platform.io/prune: disabled`.
#[tracing::instrument(skip(kube_client, orphans), err)]
pub async fn prune_orphans(
    kube_client: &kube::Client,
    orphans: &[TrackedResource],
) -> Result<usize, DeployerError> {
    let mut deleted = 0;

    for res in orphans {
        let (group, version) = parse_api_version(&res.api_version);
        let plural = kind_to_plural(&res.kind);

        let ar = ApiResource {
            group: group.to_owned(),
            version: version.to_owned(),
            api_version: res.api_version.clone(),
            kind: res.kind.clone(),
            plural,
        };

        let api: Api<DynamicObject> =
            Api::namespaced_with(kube_client.clone(), &res.namespace, &ar);

        // Check for prune-disabled annotation before deleting
        match api.get(&res.name).await {
            Ok(obj) => {
                if has_prune_disabled(&obj) {
                    tracing::info!(
                        kind = %res.kind,
                        name = %res.name,
                        "skipping prune-disabled resource"
                    );
                    continue;
                }
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Already gone
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    kind = %res.kind,
                    name = %res.name,
                    "failed to check resource before prune"
                );
                continue;
            }
        }

        match api.delete(&res.name, &DeleteParams::default()).await {
            Ok(_) => {
                tracing::info!(
                    kind = %res.kind,
                    name = %res.name,
                    namespace = %res.namespace,
                    "orphaned resource pruned"
                );
                deleted += 1;
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Already gone — not an error
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    kind = %res.kind,
                    name = %res.name,
                    "failed to prune orphaned resource"
                );
            }
        }
    }

    Ok(deleted)
}

/// Inject `platform.io/managed-by` and `platform.io/deployment-id` labels.
fn inject_managed_labels(doc: &mut serde_json::Value, deployment_id: Uuid) {
    // R10: Ensure metadata exists defensively
    if doc.get("metadata").is_none() {
        doc["metadata"] = serde_json::json!({});
    }

    let labels = doc
        .pointer_mut("/metadata/labels")
        .and_then(|v| v.as_object_mut());

    if let Some(labels) = labels {
        labels.insert(
            "platform.io/managed-by".into(),
            serde_json::json!("platform-deployer"),
        );
        labels.insert(
            "platform.io/deployment-id".into(),
            serde_json::json!(deployment_id.to_string()),
        );
    } else {
        // Ensure metadata.labels exists
        if let Some(metadata) = doc.get_mut("metadata").and_then(|m| m.as_object_mut()) {
            metadata.insert(
                "labels".into(),
                serde_json::json!({
                    "platform.io/managed-by": "platform-deployer",
                    "platform.io/deployment-id": deployment_id.to_string(),
                }),
            );
        }
    }
}

/// Check if a resource has the `platform.io/prune: disabled` annotation.
fn has_prune_disabled(obj: &DynamicObject) -> bool {
    obj.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("platform.io/prune"))
        .is_some_and(|v| v == "disabled")
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
#[allow(dead_code)]
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

/// Inject `envFrom: [{secretRef: {name: ...}}]` into all workload containers
/// in the rendered YAML, so that deployed pods automatically receive env vars
/// from a K8s Secret (e.g. OTEL config, deploy-scoped secrets).
///
/// Processes `Deployment`, `StatefulSet`, `DaemonSet`, `Job` (pod template) and
/// `CronJob` (job template → pod template). Non-workload kinds are passed through
/// unchanged. Idempotent: skips containers that already reference the secret.
pub fn inject_env_from_secret(
    manifests_yaml: &str,
    secret_name: &str,
) -> Result<String, DeployerError> {
    let docs = renderer::split_yaml_documents(manifests_yaml);
    let mut output_docs = Vec::with_capacity(docs.len());

    for doc_str in &docs {
        let mut doc: serde_json::Value = serde_yaml::from_str(doc_str)
            .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;

        let kind = doc["kind"].as_str().unwrap_or_default();
        match kind {
            "Deployment" | "StatefulSet" | "DaemonSet" | "Job" => {
                inject_env_from_to_pod_spec(&mut doc, "/spec/template/spec", secret_name);
            }
            "CronJob" => {
                inject_env_from_to_pod_spec(
                    &mut doc,
                    "/spec/jobTemplate/spec/template/spec",
                    secret_name,
                );
            }
            _ => { /* non-workload kind — pass through */ }
        }

        let yaml_str = serde_yaml::to_string(&doc)
            .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;
        output_docs.push(yaml_str);
    }

    Ok(output_docs.join("---\n"))
}

/// Inject `envFrom` into all `containers` and `initContainers` under the given
/// JSON pointer to a pod spec.
fn inject_env_from_to_pod_spec(
    doc: &mut serde_json::Value,
    pod_spec_path: &str,
    secret_name: &str,
) {
    if let Some(spec) = doc.pointer_mut(pod_spec_path) {
        for field in &["containers", "initContainers"] {
            if let Some(containers) = spec.get_mut(*field).and_then(|v| v.as_array_mut()) {
                for container in containers.iter_mut() {
                    inject_env_from_to_container(container, secret_name);
                }
            }
        }
    }
}

/// Inject a single `envFrom` secretRef entry into a container, unless it already
/// references the same secret name.
fn inject_env_from_to_container(container: &mut serde_json::Value, secret_name: &str) {
    let entry = serde_json::json!({"secretRef": {"name": secret_name}});

    if let Some(env_from) = container.get_mut("envFrom").and_then(|v| v.as_array_mut()) {
        // Idempotent: check if this secretRef already exists
        let already = env_from.iter().any(|e| {
            e.get("secretRef")
                .and_then(|sr| sr.get("name"))
                .and_then(|n| n.as_str())
                == Some(secret_name)
        });
        if !already {
            env_from.push(entry);
        }
    } else {
        container["envFrom"] = serde_json::json!([entry]);
    }
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
        "NetworkPolicy" => "networkpolicies".into(),
        "HTTPRoute" => "httproutes".into(),
        "Gateway" => "gateways".into(),
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

    // --- TrackedResource tests ---

    #[test]
    fn tracked_resources_json_round_trip() {
        let resources = vec![
            TrackedResource {
                api_version: "apps/v1".into(),
                kind: "Deployment".into(),
                name: "myapp".into(),
                namespace: "myapp-prod".into(),
            },
            TrackedResource {
                api_version: "v1".into(),
                kind: "Service".into(),
                name: "myapp".into(),
                namespace: "myapp-prod".into(),
            },
        ];

        let json = serde_json::to_string(&resources).unwrap();
        let parsed: Vec<TrackedResource> = serde_json::from_str(&json).unwrap();
        assert_eq!(resources, parsed);
    }

    #[test]
    fn tracked_resources_equality() {
        let a = TrackedResource {
            api_version: "apps/v1".into(),
            kind: "Deployment".into(),
            name: "myapp".into(),
            namespace: "default".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn resource_diff_finds_orphans() {
        let old = vec![
            TrackedResource {
                api_version: "apps/v1".into(),
                kind: "Deployment".into(),
                name: "a".into(),
                namespace: "ns".into(),
            },
            TrackedResource {
                api_version: "v1".into(),
                kind: "Service".into(),
                name: "b".into(),
                namespace: "ns".into(),
            },
            TrackedResource {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                name: "c".into(),
                namespace: "ns".into(),
            },
        ];
        let new = vec![old[0].clone(), old[1].clone()];
        let orphans = find_orphans(&old, &new);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].name, "c");
    }

    #[test]
    fn resource_diff_empty_old_no_orphans() {
        let new = vec![TrackedResource {
            api_version: "v1".into(),
            kind: "Service".into(),
            name: "svc".into(),
            namespace: "ns".into(),
        }];
        let orphans = find_orphans(&[], &new);
        assert!(orphans.is_empty());
    }

    #[test]
    fn resource_diff_same_set_no_orphans() {
        let resources = vec![TrackedResource {
            api_version: "v1".into(),
            kind: "Service".into(),
            name: "svc".into(),
            namespace: "ns".into(),
        }];
        let orphans = find_orphans(&resources, &resources);
        assert!(orphans.is_empty());
    }

    #[test]
    fn resource_diff_all_removed() {
        let old = vec![
            TrackedResource {
                api_version: "apps/v1".into(),
                kind: "Deployment".into(),
                name: "a".into(),
                namespace: "ns".into(),
            },
            TrackedResource {
                api_version: "v1".into(),
                kind: "Service".into(),
                name: "b".into(),
                namespace: "ns".into(),
            },
        ];
        let orphans = find_orphans(&old, &[]);
        assert_eq!(orphans.len(), 2);
    }

    #[test]
    fn inject_managed_by_labels_existing_labels() {
        let deployment_id = Uuid::new_v4();
        let mut doc = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": "test",
                "labels": {
                    "app": "test"
                }
            }
        });

        inject_managed_labels(&mut doc, deployment_id);

        let labels = doc["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/managed-by"], "platform-deployer");
        assert_eq!(
            labels["platform.io/deployment-id"],
            deployment_id.to_string()
        );
        // Original label preserved
        assert_eq!(labels["app"], "test");
    }

    #[test]
    fn inject_managed_by_labels_no_existing_labels() {
        let deployment_id = Uuid::new_v4();
        let mut doc = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {
                "name": "svc"
            }
        });

        inject_managed_labels(&mut doc, deployment_id);

        let labels = doc["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/managed-by"], "platform-deployer");
        assert_eq!(
            labels["platform.io/deployment-id"],
            deployment_id.to_string()
        );
    }

    #[test]
    fn build_tracked_inventory_from_manifests() {
        let manifests = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: myapp\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: myapp-svc";
        let tracked = build_tracked_inventory(manifests, "default");
        assert_eq!(tracked.len(), 2);
        assert_eq!(tracked[0].kind, "Deployment");
        assert_eq!(tracked[0].name, "myapp");
        assert_eq!(tracked[0].namespace, "default");
        assert_eq!(tracked[1].kind, "Service");
        assert_eq!(tracked[1].name, "myapp-svc");
    }

    #[test]
    fn build_tracked_inventory_uses_resource_namespace() {
        let manifests =
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: cfg\n  namespace: custom-ns";
        let tracked = build_tracked_inventory(manifests, "default");
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].namespace, "custom-ns");
    }

    // --- R9: edge case tests for build_tracked_inventory ---

    #[test]
    fn build_tracked_inventory_empty_yaml() {
        let tracked = build_tracked_inventory("", "default");
        assert!(tracked.is_empty());
    }

    #[test]
    fn build_tracked_inventory_invalid_doc_skipped() {
        let manifests = "apiVersion: v1\nkind: Service\nmetadata:\n  name: good-svc\n---\n{{{invalid yaml\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: good-cm";
        let tracked = build_tracked_inventory(manifests, "default");
        assert_eq!(tracked.len(), 2);
        assert_eq!(tracked[0].name, "good-svc");
        assert_eq!(tracked[1].name, "good-cm");
    }

    #[test]
    fn build_tracked_inventory_missing_kind_skipped() {
        let manifests = "apiVersion: v1\nmetadata:\n  name: no-kind";
        let tracked = build_tracked_inventory(manifests, "default");
        assert!(tracked.is_empty());
    }

    #[test]
    fn build_tracked_inventory_missing_name_skipped() {
        let manifests = "apiVersion: v1\nkind: Service";
        let tracked = build_tracked_inventory(manifests, "default");
        assert!(tracked.is_empty());
    }

    // --- R10: inject_managed_labels no-metadata test ---

    #[test]
    fn inject_managed_labels_no_metadata_key() {
        let deployment_id = Uuid::new_v4();
        let mut doc = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
        });

        inject_managed_labels(&mut doc, deployment_id);

        let labels = doc["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/managed-by"], "platform-deployer");
        assert_eq!(
            labels["platform.io/deployment-id"],
            deployment_id.to_string()
        );
    }

    // --- R2: cluster-scoped kind rejection test ---

    #[test]
    fn allowed_kinds_contains_expected() {
        for kind in &[
            "Deployment",
            "Service",
            "ConfigMap",
            "Secret",
            "Ingress",
            "Job",
            "CronJob",
            "StatefulSet",
            "DaemonSet",
            "NetworkPolicy",
            "HTTPRoute",
            "Gateway",
        ] {
            assert!(
                ALLOWED_KINDS.contains(kind),
                "{kind} should be in ALLOWED_KINDS"
            );
        }
    }

    #[test]
    fn cluster_scoped_kinds_not_allowed() {
        for kind in &["ClusterRole", "ClusterRoleBinding", "Namespace"] {
            assert!(
                !ALLOWED_KINDS.contains(kind),
                "{kind} should NOT be in ALLOWED_KINDS"
            );
        }
    }

    #[test]
    fn has_prune_disabled_annotation() {
        let mut obj = DynamicObject::new(
            "test",
            &ApiResource {
                group: String::new(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                plural: "configmaps".into(),
            },
        );
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert("platform.io/prune".into(), "disabled".into());
        obj.metadata.annotations = Some(annotations);

        assert!(has_prune_disabled(&obj));
    }

    #[test]
    fn no_prune_annotation_returns_false() {
        let obj = DynamicObject::new(
            "test",
            &ApiResource {
                group: String::new(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                plural: "configmaps".into(),
            },
        );

        assert!(!has_prune_disabled(&obj));
    }

    // --- inject_env_from_secret tests ---

    #[test]
    fn inject_env_from_single_deployment() {
        let yaml = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: myapp
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest";

        let result = inject_env_from_secret(yaml, "myapp-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env_from = &doc["spec"]["template"]["spec"]["containers"][0]["envFrom"];
        assert_eq!(env_from[0]["secretRef"]["name"], "myapp-secrets");
    }

    #[test]
    fn inject_env_from_multi_doc_only_workloads() {
        let yaml = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: myapp
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
---
apiVersion: v1
kind: Service
metadata:
  name: myapp-svc
spec:
  ports:
    - port: 80";

        let result = inject_env_from_secret(yaml, "myapp-secrets").unwrap();
        let docs: Vec<&str> = result.split("---\n").collect();
        assert_eq!(docs.len(), 2);

        let deploy: serde_json::Value = serde_yaml::from_str(docs[0]).unwrap();
        assert!(
            deploy["spec"]["template"]["spec"]["containers"][0]["envFrom"][0]["secretRef"]["name"]
                .as_str()
                .is_some()
        );

        let svc: serde_json::Value = serde_yaml::from_str(docs[1]).unwrap();
        assert!(
            svc["spec"]["template"].is_null()
                || svc
                    .pointer("/spec/template/spec/containers/0/envFrom")
                    .is_none()
        );
    }

    #[test]
    fn inject_env_from_idempotent() {
        let yaml = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: myapp
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          envFrom:
            - secretRef:
                name: myapp-secrets";

        let result = inject_env_from_secret(yaml, "myapp-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env_from = doc["spec"]["template"]["spec"]["containers"][0]["envFrom"]
            .as_array()
            .unwrap();
        assert_eq!(env_from.len(), 1, "should not duplicate existing secretRef");
    }

    #[test]
    fn inject_env_from_appends_to_existing_different_secret() {
        let yaml = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: myapp
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          envFrom:
            - secretRef:
                name: other-secret";

        let result = inject_env_from_secret(yaml, "myapp-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env_from = doc["spec"]["template"]["spec"]["containers"][0]["envFrom"]
            .as_array()
            .unwrap();
        assert_eq!(env_from.len(), 2);
        assert_eq!(env_from[0]["secretRef"]["name"], "other-secret");
        assert_eq!(env_from[1]["secretRef"]["name"], "myapp-secrets");
    }

    #[test]
    fn inject_env_from_service_only_unchanged() {
        let yaml = "\
apiVersion: v1
kind: Service
metadata:
  name: myapp
spec:
  ports:
    - port: 80";

        let result = inject_env_from_secret(yaml, "myapp-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        assert!(doc.pointer("/spec/template").is_none());
    }

    #[test]
    fn inject_env_from_init_containers() {
        let yaml = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: myapp
spec:
  template:
    spec:
      initContainers:
        - name: init
          image: busybox
      containers:
        - name: app
          image: myapp:latest";

        let result = inject_env_from_secret(yaml, "myapp-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let init_env = &doc["spec"]["template"]["spec"]["initContainers"][0]["envFrom"];
        assert_eq!(init_env[0]["secretRef"]["name"], "myapp-secrets");
        let app_env = &doc["spec"]["template"]["spec"]["containers"][0]["envFrom"];
        assert_eq!(app_env[0]["secretRef"]["name"], "myapp-secrets");
    }

    #[test]
    fn inject_env_from_statefulset() {
        let yaml = "\
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: mydb
spec:
  template:
    spec:
      containers:
        - name: db
          image: postgres:16";

        let result = inject_env_from_secret(yaml, "db-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        assert_eq!(
            doc["spec"]["template"]["spec"]["containers"][0]["envFrom"][0]["secretRef"]["name"],
            "db-secrets"
        );
    }

    #[test]
    fn inject_env_from_cronjob() {
        let yaml = "\
apiVersion: batch/v1
kind: CronJob
metadata:
  name: backup
spec:
  schedule: '0 2 * * *'
  jobTemplate:
    spec:
      template:
        spec:
          containers:
            - name: backup
              image: backup:latest";

        let result = inject_env_from_secret(yaml, "backup-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env_from =
            &doc["spec"]["jobTemplate"]["spec"]["template"]["spec"]["containers"][0]["envFrom"];
        assert_eq!(env_from[0]["secretRef"]["name"], "backup-secrets");
    }
}
