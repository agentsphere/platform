// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

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
    // Gateway API: only HTTPRoute (users reference existing Gateways via parentRefs,
    // not create Gateways which could capture cross-tenant traffic)
    "HTTPRoute",
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

        // Skip non-manifest YAML docs (e.g. variables files without apiVersion/kind)
        if doc.get("apiVersion").and_then(|v| v.as_str()).is_none() {
            tracing::debug!(doc = %doc_str.chars().take(100).collect::<String>(), "skipping non-manifest YAML doc");
            continue;
        }

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

        // R3: Reject manifests with dangerous pod specs (S19 security hardening)
        validate_pod_spec(&doc)?;

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

/// Extract the pod spec from a workload manifest (`Deployment`, `StatefulSet`, `DaemonSet`,
/// `Job`, `CronJob`). Returns `None` for non-workload kinds (`Service`, `ConfigMap`, etc.).
fn extract_pod_spec(manifest: &serde_json::Value) -> Option<&serde_json::Value> {
    // Deployment, StatefulSet, DaemonSet, ReplicaSet → spec.template.spec
    manifest
        .pointer("/spec/template/spec")
        // CronJob → spec.jobTemplate.spec.template.spec
        .or_else(|| manifest.pointer("/spec/jobTemplate/spec/template/spec"))
}

/// Validate that a manifest's pod spec does not contain dangerous fields that would
/// allow container escape or host-level access. Blocks: privileged containers,
/// hostNetwork, hostPID, hostIPC, hostPath volumes.
fn validate_pod_spec(manifest: &serde_json::Value) -> Result<(), DeployerError> {
    let Some(spec) = extract_pod_spec(manifest) else {
        return Ok(());
    };

    if spec.pointer("/hostNetwork") == Some(&serde_json::Value::Bool(true)) {
        return Err(DeployerError::ForbiddenManifest(
            "hostNetwork is not allowed".into(),
        ));
    }
    if spec.pointer("/hostPID") == Some(&serde_json::Value::Bool(true)) {
        return Err(DeployerError::ForbiddenManifest(
            "hostPID is not allowed".into(),
        ));
    }
    if spec.pointer("/hostIPC") == Some(&serde_json::Value::Bool(true)) {
        return Err(DeployerError::ForbiddenManifest(
            "hostIPC is not allowed".into(),
        ));
    }

    // Check all containers (main + init) for privileged mode
    for containers_key in ["/containers", "/initContainers"] {
        if let Some(containers) = spec.pointer(containers_key).and_then(|c| c.as_array()) {
            for container in containers {
                if container.pointer("/securityContext/privileged")
                    == Some(&serde_json::Value::Bool(true))
                {
                    return Err(DeployerError::ForbiddenManifest(
                        "privileged containers are not allowed".into(),
                    ));
                }
            }
        }
    }

    // Check for hostPath volumes
    if let Some(volumes) = spec.pointer("/volumes").and_then(|v| v.as_array()) {
        for vol in volumes {
            if vol.get("hostPath").is_some() {
                return Err(DeployerError::ForbiddenManifest(
                    "hostPath volumes are not allowed".into(),
                ));
            }
        }
    }

    Ok(())
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

    Ok(output_docs.join("\n---\n"))
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

// ---------------------------------------------------------------------------
// Proxy injection
// ---------------------------------------------------------------------------

/// Configuration for proxy injection.
pub struct ProxyInjectionConfig {
    /// Platform API URL for proxy binary download and OTLP export.
    pub platform_api_url: String,
    /// Name of the K8s Secret in the target namespace that contains `PLATFORM_API_TOKEN`.
    /// Used by the init container to authenticate the proxy binary download.
    pub platform_secret_name: Option<String>,
    /// Image for the proxy download init container (e.g. `registry/platform-runner-bare:v1`).
    /// Must have `curl` or `wget` available. Falls back to `busybox:stable` if unset.
    pub init_image: String,
    /// Image for the iptables init container (e.g. `registry/platform-proxy-init:v1`).
    /// Shell-free distroless image with only iptables and the Rust binary.
    /// Falls back to `init_image` if empty (with shell-based fallback script).
    pub iptables_init_image: Option<String>,
    /// Enable transparent proxy mode (iptables interception).
    pub mesh_transparent: bool,
    /// Enable strict mTLS (reject plaintext from non-kubelet IPs).
    pub mesh_strict_mtls: bool,
}

/// Inject platform-proxy wrapper into all containers in workload manifests.
///
/// For each container in Deployment/StatefulSet/DaemonSet/Job/CronJob:
/// 1. Adds an init container that downloads the proxy binary from the platform API
/// 2. Wraps the container command with `/proxy/platform-proxy --wrap --`
/// 3. Adds proxy volume mount at `/proxy` (read-only, emptyDir shared with init container)
/// 4. Adds proxy env vars (`PLATFORM_API_URL`, `PLATFORM_SERVICE_NAME`)
///
/// Containers without an explicit `command` require entrypoint resolution
/// (handled separately by the caller before invoking this function).
pub fn inject_proxy_wrapper(
    manifests_yaml: &str,
    config: &ProxyInjectionConfig,
) -> Result<String, DeployerError> {
    let docs = renderer::split_yaml_documents(manifests_yaml);
    let mut output_docs = Vec::with_capacity(docs.len());

    for doc_str in &docs {
        let mut doc: serde_json::Value = serde_yaml::from_str(doc_str)
            .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;

        let kind = doc["kind"].as_str().unwrap_or_default().to_string();
        let name = doc["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        match kind.as_str() {
            "Deployment" | "StatefulSet" | "DaemonSet" | "Job" => {
                inject_proxy_to_pod_spec(&mut doc, "/spec/template/spec", config, &name);
            }
            "CronJob" => {
                inject_proxy_to_pod_spec(
                    &mut doc,
                    "/spec/jobTemplate/spec/template/spec",
                    config,
                    &name,
                );
            }
            _ => {}
        }

        let yaml_str = serde_yaml::to_string(&doc)
            .map_err(|e| DeployerError::InvalidManifest(e.to_string()))?;
        output_docs.push(yaml_str);
    }

    Ok(output_docs.join("\n---\n"))
}

/// Build the init container that downloads the proxy binary from the platform API.
fn build_proxy_init_container(config: &ProxyInjectionConfig) -> serde_json::Value {
    // Download script: detect arch, download proxy binary via curl, make executable.
    let download_script = r#"set -eu
ARCH=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
echo "[proxy-init] Downloading platform-proxy ($ARCH)..."
AUTH_HEADER=""
if [ -n "${PLATFORM_API_TOKEN:-}" ]; then
  AUTH_HEADER="Authorization: Bearer $PLATFORM_API_TOKEN"
fi
if command -v curl >/dev/null 2>&1; then
  curl -sf ${AUTH_HEADER:+-H "$AUTH_HEADER"} \
    "${PLATFORM_API_URL}/api/downloads/platform-proxy?arch=${ARCH}" \
    -o /proxy/platform-proxy
elif command -v wget >/dev/null 2>&1; then
  wget -q ${AUTH_HEADER:+--header="$AUTH_HEADER"} \
    "${PLATFORM_API_URL}/api/downloads/platform-proxy?arch=${ARCH}" \
    -O /proxy/platform-proxy
else
  echo "[proxy-init] ERROR: need curl or wget" >&2; exit 1
fi
chmod +x /proxy/platform-proxy
echo "[proxy-init] platform-proxy downloaded""#;

    let mut env =
        vec![serde_json::json!({"name": "PLATFORM_API_URL", "value": config.platform_api_url})];

    // If we have a platform secret, pull PLATFORM_API_TOKEN from it
    if let Some(ref secret_name) = config.platform_secret_name {
        env.push(serde_json::json!({
            "name": "PLATFORM_API_TOKEN",
            "valueFrom": {
                "secretKeyRef": {
                    "name": secret_name,
                    "key": "PLATFORM_API_TOKEN",
                    "optional": true
                }
            }
        }));
    }

    serde_json::json!({
        "name": "proxy-init",
        "image": config.init_image,
        "command": ["sh", "-c"],
        "args": [download_script],
        "env": env,
        "volumeMounts": [{
            "name": "platform-proxy",
            "mountPath": "/proxy"
        }],
        "resources": {
            "requests": {"cpu": "50m", "memory": "32Mi"},
            "limits": {"cpu": "200m", "memory": "64Mi"}
        }
    })
}

/// Build the iptables init container that sets up REDIRECT rules for transparent proxy.
///
/// When `iptables_init_image` is set, uses the shell-free distroless image
/// (`platform-proxy-init`) — no `/bin/sh`, just the static Rust binary + iptables.
/// Falls back to a shell script on the generic init image otherwise.
fn build_proxy_iptables_init_container(config: &ProxyInjectionConfig) -> serde_json::Value {
    let security_context = serde_json::json!({
        "capabilities": {
            "add": ["NET_ADMIN", "NET_RAW"],
            "drop": ["ALL"]
        },
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "runAsUser": 0
    });
    let resources = serde_json::json!({
        "requests": { "cpu": "10m", "memory": "16Mi" },
        "limits": { "cpu": "100m", "memory": "32Mi" }
    });

    if let Some(ref image) = config.iptables_init_image {
        // Distroless: entrypoint is /platform-proxy-init, no shell needed
        serde_json::json!({
            "name": "proxy-iptables",
            "image": image,
            "securityContext": security_context,
            "resources": resources
        })
    } else {
        // Fallback: shell-based script (requires iptables in the image)
        let script = r#"set -eu
apk add --no-cache iptables >/dev/null 2>&1 || true
PROXY_INBOUND_PORT=${PROXY_INBOUND_PORT:-15006}
PROXY_OUTBOUND_PORT=${PROXY_OUTBOUND_PORT:-15001}
PROXY_HEALTH_PORT=${PROXY_HEALTH_PORT:-15020}
PROXY_OUTBOUND_BIND=${PROXY_OUTBOUND_BIND:-127.0.0.6}

iptables -t nat -N PLATFORM_INBOUND
iptables -t nat -A PLATFORM_INBOUND -p tcp --dport ${PROXY_INBOUND_PORT} -j RETURN
iptables -t nat -A PLATFORM_INBOUND -p tcp --dport ${PROXY_OUTBOUND_PORT} -j RETURN
iptables -t nat -A PLATFORM_INBOUND -p tcp --dport ${PROXY_HEALTH_PORT} -j RETURN
iptables -t nat -A PLATFORM_INBOUND -p tcp -j REDIRECT --to-ports ${PROXY_INBOUND_PORT}
iptables -t nat -A PREROUTING -p tcp -j PLATFORM_INBOUND

iptables -t nat -N PLATFORM_OUTPUT
iptables -t nat -A PLATFORM_OUTPUT -s ${PROXY_OUTBOUND_BIND}/32 -j RETURN
iptables -t nat -A PLATFORM_OUTPUT -o lo -d 127.0.0.1/32 -j RETURN
iptables -t nat -A PLATFORM_OUTPUT -p tcp --dport 53 -j RETURN
iptables -t nat -A PLATFORM_OUTPUT -p tcp -j REDIRECT --to-ports ${PROXY_OUTBOUND_PORT}
iptables -t nat -A OUTPUT -p tcp -j PLATFORM_OUTPUT

echo "[proxy-iptables] rules installed (inbound:${PROXY_INBOUND_PORT} outbound:${PROXY_OUTBOUND_PORT})"
"#;
        serde_json::json!({
            "name": "proxy-iptables",
            "image": config.init_image,
            "command": ["sh", "-c"],
            "args": [script],
            "securityContext": security_context,
            "resources": resources
        })
    }
}

/// Inject proxy wrapping into all containers under a pod spec path.
fn inject_proxy_to_pod_spec(
    doc: &mut serde_json::Value,
    pod_spec_path: &str,
    config: &ProxyInjectionConfig,
    workload_name: &str,
) {
    let Some(spec) = doc.pointer_mut(pod_spec_path) else {
        return;
    };

    // Check if any container has a command to wrap (skip if none do)
    let has_wrappable = spec
        .get("containers")
        .and_then(|v| v.as_array())
        .is_some_and(|containers| {
            containers.iter().any(|c| {
                c.get("command")
                    .and_then(|v| v.as_array())
                    .is_some_and(|a| !a.is_empty())
            })
        });
    if !has_wrappable {
        return;
    }

    // Add emptyDir proxy volume
    let proxy_volume = serde_json::json!({
        "name": "platform-proxy",
        "emptyDir": { "sizeLimit": "50Mi" }
    });

    let volumes = spec.get_mut("volumes").and_then(|v| v.as_array_mut());
    if let Some(vols) = volumes {
        if !vols.iter().any(|v| v["name"] == "platform-proxy") {
            vols.push(proxy_volume);
        }
    } else {
        spec["volumes"] = serde_json::json!([proxy_volume]);
    }

    // Add proxy download init container
    let init_container = build_proxy_init_container(config);
    if let Some(init_containers) = spec
        .get_mut("initContainers")
        .and_then(|v| v.as_array_mut())
    {
        if !init_containers.iter().any(|c| c["name"] == "proxy-init") {
            init_containers.push(init_container);
        }
    } else {
        spec["initContainers"] = serde_json::json!([init_container]);
    }

    // Add iptables init container for transparent mode (after proxy-init)
    if config.mesh_transparent {
        let iptables_container = build_proxy_iptables_init_container(config);
        if let Some(init_containers) = spec
            .get_mut("initContainers")
            .and_then(|v| v.as_array_mut())
            && !init_containers
                .iter()
                .any(|c| c["name"] == "proxy-iptables")
        {
            init_containers.push(iptables_container);
        }
    }

    // Wrap each container
    if let Some(containers) = spec.get_mut("containers").and_then(|v| v.as_array_mut()) {
        for container in containers.iter_mut() {
            inject_proxy_to_container(container, config, workload_name);
        }
    }
}

/// Wrap a single container with the proxy command.
fn inject_proxy_to_container(
    container: &mut serde_json::Value,
    config: &ProxyInjectionConfig,
    workload_name: &str,
) {
    let container_name = container["name"].as_str().unwrap_or("main").to_string();

    // Get existing command + args
    let existing_command: Vec<String> = container
        .get("command")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let existing_args: Vec<String> = container
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Skip containers without explicit command (can't wrap unknown entrypoint)
    // The caller should resolve entrypoints and set command before calling this.
    if existing_command.is_empty() {
        return;
    }

    // Build wrapped command: /proxy/platform-proxy --wrap -- <original command + args>
    let mut proxy_args = vec!["--wrap".to_string(), "--".to_string()];
    proxy_args.extend(existing_command);
    proxy_args.extend(existing_args);

    container["command"] = serde_json::json!(["/proxy/platform-proxy"]);
    container["args"] = serde_json::json!(proxy_args);

    // Add proxy volume mount
    let proxy_mount = serde_json::json!({
        "name": "platform-proxy",
        "mountPath": "/proxy",
        "readOnly": true
    });
    if let Some(mounts) = container
        .get_mut("volumeMounts")
        .and_then(|v| v.as_array_mut())
    {
        if !mounts.iter().any(|m| m["name"] == "platform-proxy") {
            mounts.push(proxy_mount);
        }
    } else {
        container["volumeMounts"] = serde_json::json!([proxy_mount]);
    }

    // Add proxy env vars
    let service_name = format!("{workload_name}/{container_name}");
    let mut proxy_env = vec![
        serde_json::json!({"name": "PLATFORM_API_URL", "value": config.platform_api_url}),
        serde_json::json!({"name": "PLATFORM_SERVICE_NAME", "value": service_name}),
        serde_json::json!({"name": "PROXY_HEALTH_PORT", "value": "15020"}),
    ];

    // Transparent proxy env vars
    if config.mesh_transparent {
        proxy_env.push(serde_json::json!({"name": "PROXY_TRANSPARENT", "value": "true"}));
        let mtls_mode = if config.mesh_strict_mtls {
            "strict"
        } else {
            "permissive"
        };
        proxy_env.push(serde_json::json!({"name": "PROXY_MTLS_MODE", "value": mtls_mode}));
        proxy_env.push(serde_json::json!({"name": "PROXY_INBOUND_PORT", "value": "15006"}));
        proxy_env.push(serde_json::json!({"name": "PROXY_OUTBOUND_BIND", "value": "127.0.0.6"}));
        proxy_env.push(serde_json::json!({"name": "PROXY_INTERNAL_CIDRS", "value": "10.0.0.0/8,172.16.0.0/12,192.168.0.0/16"}));
    }

    if let Some(env) = container.get_mut("env").and_then(|v| v.as_array_mut()) {
        for e in &proxy_env {
            let name = e["name"].as_str().unwrap_or_default();
            if !env.iter().any(|existing| existing["name"] == name) {
                env.push(e.clone());
            }
        }
    } else {
        container["env"] = serde_json::json!(proxy_env);
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
        ] {
            assert!(
                ALLOWED_KINDS.contains(kind),
                "{kind} should be in ALLOWED_KINDS"
            );
        }
    }

    #[test]
    fn gateway_kind_not_allowed() {
        // A18: Gateway removed to prevent cross-tenant traffic capture
        assert!(
            !ALLOWED_KINDS.contains(&"Gateway"),
            "Gateway should NOT be in ALLOWED_KINDS"
        );
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

    // -- validate_pod_spec (S19 security hardening) --

    #[test]
    fn validate_pod_spec_rejects_privileged() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [{
                    "name": "evil",
                    "image": "alpine",
                    "securityContext": { "privileged": true }
                }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("privileged")));
    }

    #[test]
    fn validate_pod_spec_rejects_host_network() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "hostNetwork": true,
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(
            matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostNetwork"))
        );
    }

    #[test]
    fn validate_pod_spec_rejects_host_pid() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "spec": { "template": { "spec": {
                "hostPID": true,
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostPID")));
    }

    #[test]
    fn validate_pod_spec_rejects_host_ipc() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "hostIPC": true,
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostIPC")));
    }

    #[test]
    fn validate_pod_spec_rejects_host_path() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [{ "name": "app", "image": "nginx" }],
                "volumes": [{ "name": "root", "hostPath": { "path": "/" } }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostPath")));
    }

    #[test]
    fn validate_pod_spec_rejects_privileged_init_container() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "initContainers": [{
                    "name": "init-evil",
                    "image": "alpine",
                    "securityContext": { "privileged": true }
                }],
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("privileged")));
    }

    #[test]
    fn validate_pod_spec_allows_normal_deployment() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [{
                    "name": "app",
                    "image": "nginx:1.25",
                    "securityContext": { "readOnlyRootFilesystem": true }
                }],
                "volumes": [
                    { "name": "config", "configMap": { "name": "app-config" } },
                    { "name": "data", "secret": { "secretName": "app-secret" } }
                ]
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_allows_non_workload() {
        // Service, ConfigMap, etc. have no pod spec — should pass
        let manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "spec": { "ports": [{ "port": 80 }] }
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_cronjob_nested() {
        let manifest = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": {
                "hostNetwork": true,
                "containers": [{ "name": "cron", "image": "alpine" }]
            }}}}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(
            matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostNetwork"))
        );
    }

    // -- validate_pod_spec: explicit false values should pass --

    #[test]
    fn validate_pod_spec_host_network_false_passes() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "hostNetwork": false,
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_host_pid_false_passes() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "hostPID": false,
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_host_ipc_false_passes() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "hostIPC": false,
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_privileged_false_passes() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [{
                    "name": "app",
                    "image": "nginx",
                    "securityContext": { "privileged": false }
                }]
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_safe_volumes_pass() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [{ "name": "app", "image": "nginx" }],
                "volumes": [
                    { "name": "config", "configMap": { "name": "app-config" } },
                    { "name": "data", "emptyDir": {} },
                    { "name": "secrets", "secret": { "secretName": "my-secret" } },
                    { "name": "pvc", "persistentVolumeClaim": { "claimName": "data-pvc" } }
                ]
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_no_containers_key_passes() {
        // Pod spec with no containers array (unusual but shouldn't crash)
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {} }}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_empty_containers_passes() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": []
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_cronjob_privileged_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": {
                "containers": [{
                    "name": "evil",
                    "image": "alpine",
                    "securityContext": { "privileged": true }
                }]
            }}}}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("privileged")));
    }

    #[test]
    fn validate_pod_spec_cronjob_host_path_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": {
                "containers": [{ "name": "app", "image": "alpine" }],
                "volumes": [{ "name": "root", "hostPath": { "path": "/" } }]
            }}}}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostPath")));
    }

    #[test]
    fn validate_pod_spec_cronjob_safe_passes() {
        let manifest = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": {
                "containers": [{ "name": "backup", "image": "backup:latest" }]
            }}}}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_cronjob_host_pid_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": {
                "hostPID": true,
                "containers": [{ "name": "cron", "image": "alpine" }]
            }}}}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostPID")));
    }

    #[test]
    fn validate_pod_spec_cronjob_host_ipc_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": {
                "hostIPC": true,
                "containers": [{ "name": "cron", "image": "alpine" }]
            }}}}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostIPC")));
    }

    // -- extract_pod_spec --

    #[test]
    fn extract_pod_spec_deployment() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [{ "name": "app", "image": "nginx" }]
            }}}
        });
        let spec = extract_pod_spec(&manifest);
        assert!(spec.is_some());
        assert!(spec.unwrap().get("containers").is_some());
    }

    #[test]
    fn extract_pod_spec_cronjob() {
        let manifest = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": {
                "containers": [{ "name": "cron", "image": "alpine" }]
            }}}}}
        });
        let spec = extract_pod_spec(&manifest);
        assert!(spec.is_some());
        assert!(spec.unwrap().get("containers").is_some());
    }

    #[test]
    fn extract_pod_spec_service_returns_none() {
        let manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "spec": { "ports": [{ "port": 80 }] }
        });
        assert!(extract_pod_spec(&manifest).is_none());
    }

    #[test]
    fn extract_pod_spec_configmap_returns_none() {
        let manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": "cfg" },
            "data": { "key": "value" }
        });
        assert!(extract_pod_spec(&manifest).is_none());
    }

    // -- kind_to_plural: comprehensive coverage --

    #[test]
    fn kind_to_plural_all_allowed() {
        assert_eq!(kind_to_plural("Secret"), "secrets");
        assert_eq!(kind_to_plural("ServiceAccount"), "serviceaccounts");
        assert_eq!(
            kind_to_plural("PodDisruptionBudget"),
            "poddisruptionbudgets"
        );
        assert_eq!(kind_to_plural("Role"), "roles");
        assert_eq!(kind_to_plural("RoleBinding"), "rolebindings");
        assert_eq!(kind_to_plural("Job"), "jobs");
        assert_eq!(kind_to_plural("CronJob"), "cronjobs");
        assert_eq!(kind_to_plural("StatefulSet"), "statefulsets");
        assert_eq!(kind_to_plural("DaemonSet"), "daemonsets");
        assert_eq!(
            kind_to_plural("PersistentVolumeClaim"),
            "persistentvolumeclaims"
        );
        assert_eq!(kind_to_plural("NetworkPolicy"), "networkpolicies");
        assert_eq!(kind_to_plural("HTTPRoute"), "httproutes");
    }

    #[test]
    fn kind_to_plural_cluster_scoped() {
        assert_eq!(kind_to_plural("Namespace"), "namespaces");
        assert_eq!(kind_to_plural("ClusterRole"), "clusterroles");
        assert_eq!(kind_to_plural("ClusterRoleBinding"), "clusterrolebindings");
    }

    // -- inject_env_from_secret: Job and DaemonSet kinds --

    #[test]
    fn inject_env_from_job() {
        let yaml = "\
apiVersion: batch/v1
kind: Job
metadata:
  name: migration
spec:
  template:
    spec:
      containers:
        - name: migrate
          image: migrate:latest";

        let result = inject_env_from_secret(yaml, "job-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env_from = &doc["spec"]["template"]["spec"]["containers"][0]["envFrom"];
        assert_eq!(env_from[0]["secretRef"]["name"], "job-secrets");
    }

    #[test]
    fn inject_env_from_daemonset() {
        let yaml = "\
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: log-collector
spec:
  template:
    spec:
      containers:
        - name: collector
          image: fluentd:latest";

        let result = inject_env_from_secret(yaml, "ds-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env_from = &doc["spec"]["template"]["spec"]["containers"][0]["envFrom"];
        assert_eq!(env_from[0]["secretRef"]["name"], "ds-secrets");
    }

    #[test]
    fn inject_env_from_configmap_unchanged() {
        let yaml = "\
apiVersion: v1
kind: ConfigMap
metadata:
  name: app-config
data:
  key: value";

        let result = inject_env_from_secret(yaml, "app-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        assert_eq!(doc["kind"], "ConfigMap");
        // ConfigMap should not get envFrom injected
        assert!(doc.pointer("/spec/template").is_none());
    }

    #[test]
    fn inject_env_from_multiple_containers() {
        let yaml = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: multi
spec:
  template:
    spec:
      containers:
        - name: web
          image: web:latest
        - name: sidecar
          image: proxy:latest";

        let result = inject_env_from_secret(yaml, "multi-secrets").unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let containers = doc["spec"]["template"]["spec"]["containers"]
            .as_array()
            .unwrap();
        // Both containers should get the envFrom injection
        assert_eq!(
            containers[0]["envFrom"][0]["secretRef"]["name"],
            "multi-secrets"
        );
        assert_eq!(
            containers[1]["envFrom"][0]["secretRef"]["name"],
            "multi-secrets"
        );
    }

    // -- build_tracked_inventory edge cases --

    #[test]
    fn build_tracked_inventory_comment_only_skipped() {
        let manifests =
            "# just a comment\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: svc";
        let tracked = build_tracked_inventory(manifests, "default");
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].name, "svc");
    }

    #[test]
    fn build_tracked_inventory_multi_namespace() {
        let manifests = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
  namespace: custom-ns
---
apiVersion: v1
kind: Service
metadata:
  name: svc";
        let tracked = build_tracked_inventory(manifests, "default");
        assert_eq!(tracked.len(), 2);
        assert_eq!(tracked[0].namespace, "custom-ns");
        assert_eq!(tracked[1].namespace, "default"); // falls back to parameter
    }

    // -- has_prune_disabled: edge cases --

    #[test]
    fn has_prune_disabled_wrong_value_returns_false() {
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
        annotations.insert("platform.io/prune".into(), "enabled".into());
        obj.metadata.annotations = Some(annotations);

        assert!(!has_prune_disabled(&obj));
    }

    #[test]
    fn has_prune_disabled_other_annotations_returns_false() {
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
        annotations.insert("other/annotation".into(), "value".into());
        obj.metadata.annotations = Some(annotations);

        assert!(!has_prune_disabled(&obj));
    }

    // -- find_deployment_name: multiple deployments --

    #[test]
    fn find_deployment_name_returns_first() {
        let applied = vec![
            AppliedResource {
                kind: "Deployment".into(),
                name: "first".into(),
            },
            AppliedResource {
                kind: "Deployment".into(),
                name: "second".into(),
            },
        ];
        assert_eq!(find_deployment_name(&applied), Some("first"));
    }

    #[test]
    fn find_deployment_name_empty() {
        let applied: Vec<AppliedResource> = vec![];
        assert_eq!(find_deployment_name(&applied), None);
    }

    // -- parse_api_version: additional edge cases --

    #[test]
    fn parse_gateway_api_version() {
        let (group, version) = parse_api_version("gateway.networking.k8s.io/v1");
        assert_eq!(group, "gateway.networking.k8s.io");
        assert_eq!(version, "v1");
    }

    #[test]
    fn parse_batch_api_version() {
        let (group, version) = parse_api_version("batch/v1");
        assert_eq!(group, "batch");
        assert_eq!(version, "v1");
    }

    #[test]
    fn parse_rbac_api_version() {
        let (group, version) = parse_api_version("rbac.authorization.k8s.io/v1");
        assert_eq!(group, "rbac.authorization.k8s.io");
        assert_eq!(version, "v1");
    }

    // -- inject_managed_labels: edge cases --

    #[test]
    fn inject_managed_labels_preserves_existing_metadata_fields() {
        let deployment_id = Uuid::new_v4();
        let mut doc = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default",
                "annotations": { "note": "keep" }
            }
        });

        inject_managed_labels(&mut doc, deployment_id);

        // Original metadata preserved
        assert_eq!(doc["metadata"]["name"], "test");
        assert_eq!(doc["metadata"]["namespace"], "default");
        assert_eq!(doc["metadata"]["annotations"]["note"], "keep");
        // Labels added
        let labels = doc["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/managed-by"], "platform-deployer");
    }

    // -- api_resource_from_yaml: additional resource types --

    #[test]
    fn api_resource_from_configmap_yaml() {
        let doc = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test-cm"},
            "data": {"key": "value"}
        });

        let (ar, obj) = api_resource_from_yaml(&doc).unwrap();
        assert_eq!(ar.group, "");
        assert_eq!(ar.kind, "ConfigMap");
        assert_eq!(ar.plural, "configmaps");
        assert_eq!(obj.metadata.name.as_deref(), Some("test-cm"));
    }

    #[test]
    fn api_resource_from_cronjob_yaml() {
        let doc = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "metadata": {"name": "backup"},
            "spec": { "schedule": "0 2 * * *" }
        });

        let (ar, _) = api_resource_from_yaml(&doc).unwrap();
        assert_eq!(ar.group, "batch");
        assert_eq!(ar.version, "v1");
        assert_eq!(ar.kind, "CronJob");
        assert_eq!(ar.plural, "cronjobs");
    }

    #[test]
    fn api_resource_from_httproute_yaml() {
        let doc = serde_json::json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": {"name": "my-route"},
            "spec": {}
        });

        let (ar, _) = api_resource_from_yaml(&doc).unwrap();
        assert_eq!(ar.group, "gateway.networking.k8s.io");
        assert_eq!(ar.kind, "HTTPRoute");
        assert_eq!(ar.plural, "httproutes");
    }

    // -- validate_pod_spec: multiple containers with mixed security --

    #[test]
    fn validate_pod_spec_second_container_privileged_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [
                    { "name": "safe", "image": "nginx", "securityContext": { "privileged": false } },
                    { "name": "evil", "image": "alpine", "securityContext": { "privileged": true } }
                ]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("privileged")));
    }

    #[test]
    fn validate_pod_spec_multiple_safe_containers_pass() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [
                    { "name": "app", "image": "nginx" },
                    { "name": "sidecar", "image": "envoy" },
                    { "name": "metrics", "image": "prom-exporter" }
                ]
            }}}
        });
        assert!(validate_pod_spec(&manifest).is_ok());
    }

    #[test]
    fn validate_pod_spec_multiple_host_path_volumes_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": {
                "containers": [{ "name": "app", "image": "nginx" }],
                "volumes": [
                    { "name": "safe", "emptyDir": {} },
                    { "name": "bad", "hostPath": { "path": "/var/log" } }
                ]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostPath")));
    }

    // -- validate_pod_spec: StatefulSet and DaemonSet --

    #[test]
    fn validate_pod_spec_statefulset_privileged_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "spec": { "template": { "spec": {
                "containers": [{
                    "name": "db",
                    "image": "postgres",
                    "securityContext": { "privileged": true }
                }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("privileged")));
    }

    #[test]
    fn validate_pod_spec_daemonset_host_network_rejected() {
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "DaemonSet",
            "spec": { "template": { "spec": {
                "hostNetwork": true,
                "containers": [{ "name": "collector", "image": "fluentd" }]
            }}}
        });
        let err = validate_pod_spec(&manifest).unwrap_err();
        assert!(
            matches!(err, DeployerError::ForbiddenManifest(msg) if msg.contains("hostNetwork"))
        );
    }

    // -- inject_env_from_to_container: edge cases --

    #[test]
    fn inject_env_from_to_container_no_existing_env_from() {
        let mut container = serde_json::json!({
            "name": "app",
            "image": "nginx:latest"
        });
        inject_env_from_to_container(&mut container, "my-secret");
        let env_from = container["envFrom"].as_array().unwrap();
        assert_eq!(env_from.len(), 1);
        assert_eq!(env_from[0]["secretRef"]["name"], "my-secret");
    }

    #[test]
    fn inject_env_from_to_container_existing_configmap_ref() {
        let mut container = serde_json::json!({
            "name": "app",
            "image": "nginx:latest",
            "envFrom": [{ "configMapRef": { "name": "app-config" } }]
        });
        inject_env_from_to_container(&mut container, "my-secret");
        let env_from = container["envFrom"].as_array().unwrap();
        assert_eq!(env_from.len(), 2);
        assert_eq!(env_from[0]["configMapRef"]["name"], "app-config");
        assert_eq!(env_from[1]["secretRef"]["name"], "my-secret");
    }

    #[test]
    fn inject_env_from_to_container_already_has_same_secret() {
        let mut container = serde_json::json!({
            "name": "app",
            "image": "nginx:latest",
            "envFrom": [{ "secretRef": { "name": "my-secret" } }]
        });
        inject_env_from_to_container(&mut container, "my-secret");
        let env_from = container["envFrom"].as_array().unwrap();
        // Should not duplicate
        assert_eq!(env_from.len(), 1);
    }

    // -- inject_proxy_wrapper tests --

    fn proxy_config() -> ProxyInjectionConfig {
        ProxyInjectionConfig {
            platform_api_url: "http://platform:8080".into(),
            platform_secret_name: Some("my-platform-secret".into()),
            init_image: "busybox:stable".into(),
            iptables_init_image: None,
            mesh_transparent: false,
            mesh_strict_mtls: false,
        }
    }

    #[test]
    fn inject_proxy_wraps_deployment_command() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: my-app:v1
          command: ["python"]
          args: ["app.py"]
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let container = &doc["spec"]["template"]["spec"]["containers"][0];

        assert_eq!(container["command"][0], "/proxy/platform-proxy");
        let args: Vec<&str> = container["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args, vec!["--wrap", "--", "python", "app.py"]);
    }

    #[test]
    fn inject_proxy_adds_volume_and_mount() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: my-app:v1
          command: ["python"]
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

        // emptyDir volume added (not hostPath)
        let volumes = doc["spec"]["template"]["spec"]["volumes"]
            .as_array()
            .unwrap();
        let proxy_vol = volumes
            .iter()
            .find(|v| v["name"] == "platform-proxy")
            .unwrap();
        assert!(!proxy_vol["emptyDir"].is_null(), "should be emptyDir");
        assert!(proxy_vol["hostPath"].is_null(), "must not use hostPath");

        // Mount added
        let mounts = doc["spec"]["template"]["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap();
        assert!(
            mounts
                .iter()
                .any(|m| m["name"] == "platform-proxy" && m["mountPath"] == "/proxy")
        );

        // Init container added
        let init_containers = doc["spec"]["template"]["spec"]["initContainers"]
            .as_array()
            .unwrap();
        assert!(init_containers.iter().any(|c| c["name"] == "proxy-init"));
    }

    #[test]
    fn inject_proxy_skips_service() {
        let yaml = r#"
apiVersion: v1
kind: Service
metadata:
  name: my-svc
spec:
  ports:
    - port: 80
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        // No volumes key added to a Service
        assert!(doc["spec"]["template"].is_null());
    }

    #[test]
    fn inject_proxy_skips_container_without_command() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: my-app:v1
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let container = &doc["spec"]["template"]["spec"]["containers"][0];
        // No command wrapping — original has no command
        assert!(container["command"].is_null());
        // No init container or volume added either
        assert!(doc["spec"]["template"]["spec"]["initContainers"].is_null());
        assert!(doc["spec"]["template"]["spec"]["volumes"].is_null());
    }

    #[test]
    fn inject_proxy_adds_env_vars() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      containers:
        - name: app
          image: web:v1
          command: ["node"]
          args: ["server.js"]
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env = doc["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();

        let names: Vec<&str> = env.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"PLATFORM_API_URL"));
        assert!(names.contains(&"PLATFORM_SERVICE_NAME"));
        assert!(names.contains(&"PROXY_HEALTH_PORT"));

        // Service name is workload/container
        let svc = env
            .iter()
            .find(|e| e["name"] == "PLATFORM_SERVICE_NAME")
            .unwrap();
        assert_eq!(svc["value"], "web/app");
    }

    #[test]
    fn inject_proxy_preserves_existing_volumes() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: my-app:v1
          command: ["python"]
          volumeMounts:
            - name: data
              mountPath: /data
      volumes:
        - name: data
          emptyDir: {}
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

        let volumes = doc["spec"]["template"]["spec"]["volumes"]
            .as_array()
            .unwrap();
        assert_eq!(volumes.len(), 2); // data + platform-proxy
        assert!(volumes.iter().any(|v| v["name"] == "data"));
        assert!(volumes.iter().any(|v| v["name"] == "platform-proxy"));

        let mounts = doc["spec"]["template"]["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap();
        assert_eq!(mounts.len(), 2); // data + proxy
    }

    #[test]
    fn inject_proxy_multi_container() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: web
          image: web:v1
          command: ["node"]
        - name: worker
          image: worker:v1
          command: ["python"]
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let containers = doc["spec"]["template"]["spec"]["containers"]
            .as_array()
            .unwrap();

        // Both containers wrapped
        for c in containers {
            assert_eq!(c["command"][0], "/proxy/platform-proxy");
        }
    }

    #[test]
    fn inject_proxy_statefulset() {
        let yaml = r#"
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: db
spec:
  template:
    spec:
      containers:
        - name: postgres
          image: postgres:16
          command: ["docker-entrypoint.sh"]
          args: ["postgres"]
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let container = &doc["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(container["command"][0], "/proxy/platform-proxy");
        let args: Vec<&str> = container["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            args,
            vec!["--wrap", "--", "docker-entrypoint.sh", "postgres"]
        );
    }

    #[test]
    fn inject_proxy_multidoc_service_no_resources() {
        // Reproduces the valkey.yaml template: Deployment + Service in one file.
        // The Service must NOT get a `resources` field after round-trip.
        let yaml = r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: demo-cache
spec:
  replicas: 1
  selector:
    matchLabels:
      app: demo-cache
  template:
    metadata:
      labels:
        app: demo-cache
    spec:
      containers:
        - name: valkey
          image: valkey/valkey:8-alpine
          command: ["docker-entrypoint.sh"]
          args: ["valkey-server"]
          ports:
            - containerPort: 6379
          resources:
            requests:
              cpu: 50m
              memory: 64Mi
            limits:
              cpu: 200m
              memory: 128Mi
---
apiVersion: v1
kind: Service
metadata:
  name: demo-cache
spec:
  selector:
    app: demo-cache
  ports:
    - port: 6379
      targetPort: 6379
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();

        // Re-split and verify Service doesn't have resources
        let docs = crate::deployer::renderer::split_yaml_documents(&result);
        for doc_str in &docs {
            let doc: serde_json::Value = serde_yaml::from_str(doc_str).unwrap();
            if doc["kind"] == "Service" {
                assert!(
                    doc.get("resources").is_none(),
                    "Service should NOT have top-level resources field. Got:\n{doc_str}"
                );
                // Also check spec level
                assert!(
                    doc["spec"].get("resources").is_none(),
                    "Service spec should NOT have resources field. Got:\n{doc_str}"
                );
            }
        }
    }

    #[test]
    fn inject_proxy_iptables_init_container_when_transparent() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          command: ["./app"]
"#;
        let mut config = proxy_config();
        config.mesh_transparent = true;
        let result = inject_proxy_wrapper(yaml, &config).unwrap();
        assert!(
            result.contains("proxy-iptables"),
            "should have iptables init container"
        );
        assert!(
            result.contains("NET_ADMIN"),
            "should have NET_ADMIN capability"
        );
        assert!(
            result.contains("PROXY_TRANSPARENT"),
            "should have transparent env var"
        );
    }

    #[test]
    fn inject_proxy_iptables_distroless_image() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          command: ["./app"]
"#;
        let mut config = proxy_config();
        config.mesh_transparent = true;
        config.iptables_init_image = Some("registry/platform-proxy-init:v1".into());
        let result = inject_proxy_wrapper(yaml, &config).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let inits = doc["spec"]["template"]["spec"]["initContainers"]
            .as_array()
            .unwrap();
        let iptables = inits
            .iter()
            .find(|c| c["name"] == "proxy-iptables")
            .unwrap();
        assert_eq!(iptables["image"], "registry/platform-proxy-init:v1");
        // Distroless: no command/args (ENTRYPOINT handles it)
        assert!(
            iptables.get("command").is_none(),
            "distroless should have no command"
        );
        assert!(
            iptables.get("args").is_none(),
            "distroless should have no args"
        );
        assert_eq!(iptables["securityContext"]["runAsUser"], 0);
    }

    #[test]
    fn inject_proxy_no_iptables_when_not_transparent() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          command: ["./app"]
"#;
        let result = inject_proxy_wrapper(yaml, &proxy_config()).unwrap();
        assert!(
            !result.contains("proxy-iptables"),
            "should NOT have iptables init container"
        );
        assert!(
            !result.contains("PROXY_TRANSPARENT"),
            "should NOT have transparent env var"
        );
    }

    #[test]
    fn inject_proxy_iptables_ordering() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          command: ["./app"]
"#;
        let mut config = proxy_config();
        config.mesh_transparent = true;
        let result = inject_proxy_wrapper(yaml, &config).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let inits = doc["spec"]["template"]["spec"]["initContainers"]
            .as_array()
            .unwrap();
        assert_eq!(inits[0]["name"], "proxy-init", "proxy-init should be first");
        assert_eq!(
            inits[1]["name"], "proxy-iptables",
            "proxy-iptables should be second"
        );
    }

    #[test]
    fn inject_proxy_strict_mtls_env() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          command: ["./app"]
"#;
        let mut config = proxy_config();
        config.mesh_transparent = true;
        config.mesh_strict_mtls = true;
        let result = inject_proxy_wrapper(yaml, &config).unwrap();
        assert!(result.contains("strict"), "should have strict mTLS mode");
        // Verify it's specifically the PROXY_MTLS_MODE env var
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env = doc["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        let mtls_env = env
            .iter()
            .find(|e| e["name"] == "PROXY_MTLS_MODE")
            .expect("PROXY_MTLS_MODE env should exist");
        assert_eq!(mtls_env["value"], "strict");
    }

    #[test]
    fn inject_proxy_permissive_mtls_env() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          command: ["./app"]
"#;
        let mut config = proxy_config();
        config.mesh_transparent = true;
        config.mesh_strict_mtls = false;
        let result = inject_proxy_wrapper(yaml, &config).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env = doc["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        let mtls_env = env
            .iter()
            .find(|e| e["name"] == "PROXY_MTLS_MODE")
            .expect("PROXY_MTLS_MODE env should exist");
        assert_eq!(mtls_env["value"], "permissive");
    }

    #[test]
    fn inject_proxy_transparent_env_vars_complete() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        - name: app
          image: myapp:latest
          command: ["./app"]
"#;
        let mut config = proxy_config();
        config.mesh_transparent = true;
        let result = inject_proxy_wrapper(yaml, &config).unwrap();
        let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();
        let env = doc["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = env.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"PROXY_TRANSPARENT"));
        assert!(names.contains(&"PROXY_MTLS_MODE"));
        assert!(names.contains(&"PROXY_INBOUND_PORT"));
        assert!(names.contains(&"PROXY_OUTBOUND_BIND"));
        assert!(names.contains(&"PROXY_INTERNAL_CIDRS"));
    }
}
