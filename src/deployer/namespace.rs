use serde_json::json;

use crate::config::Config;

/// Compute the session namespace name for an agent session.
///
/// Format: `{slug}-s-{short_id}` (or `{prefix}-{slug}-s-{short_id}` with `ns_prefix`).
/// With 40-char slug max + `-s-` + 8-char ID = 51 chars max (safe under 63).
pub fn session_namespace_name(config: &Config, slug: &str, short_id: &str) -> String {
    match config.ns_prefix.as_deref() {
        Some(prefix) => format!("{prefix}-{slug}-s-{short_id}"),
        None => format!("{slug}-s-{short_id}"),
    }
}

/// Generate a per-pipeline ephemeral namespace name (`{slug}-p-{short_id}`).
pub fn pipeline_namespace_name(config: &Config, slug: &str, short_id: &str) -> String {
    match config.ns_prefix.as_deref() {
        Some(prefix) => format!("{prefix}-{slug}-p-{short_id}"),
        None => format!("{slug}-p-{short_id}"),
    }
}

/// Generate a per-test ephemeral namespace name (`{slug}-t-{short_id}`).
pub fn test_namespace_name(config: &Config, slug: &str, short_id: &str) -> String {
    match config.ns_prefix.as_deref() {
        Some(prefix) => format!("{prefix}-{slug}-t-{short_id}"),
        None => format!("{slug}-t-{short_id}"),
    }
}

/// Build K8s RBAC objects (`ServiceAccount`, `Role`, `RoleBinding`) for an agent session namespace.
///
/// Returns 3 JSON objects for server-side apply:
/// - `ServiceAccount` `agent-sa`
/// - `Role` `agent-edit` with permissions for core, apps, and batch resources
/// - `RoleBinding` `agent-edit-binding` linking SA to `Role`
///
/// Explicitly excludes `networking.k8s.io` (no `NetworkPolicy` modification).
pub fn build_session_rbac(
    ns_name: &str,
) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
    let sa = json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {
            "name": "agent-sa",
            "namespace": ns_name
        }
    });

    let role = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "Role",
        "metadata": {
            "name": "agent-edit",
            "namespace": ns_name
        },
        "rules": [
            {
                "apiGroups": [""],
                "resources": [
                    "pods", "pods/log", "pods/exec",
                    "services", "configmaps", "secrets",
                    "persistentvolumeclaims", "serviceaccounts", "events"
                ],
                "verbs": ["*"]
            },
            {
                "apiGroups": ["apps"],
                "resources": ["deployments", "statefulsets", "daemonsets", "replicasets"],
                "verbs": ["*"]
            },
            {
                "apiGroups": ["batch"],
                "resources": ["jobs", "cronjobs"],
                "verbs": ["*"]
            }
        ]
    });

    let rb = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {
            "name": "agent-edit-binding",
            "namespace": ns_name
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "Role",
            "name": "agent-edit"
        },
        "subjects": [{
            "kind": "ServiceAccount",
            "name": "agent-sa",
            "namespace": ns_name
        }]
    });

    (sa, role, rb)
}

/// Ensure a session namespace exists with RBAC and `NetworkPolicy`.
///
/// Creates: Namespace + `NetworkPolicy` (unless `dev_mode`) + `ServiceAccount` + `Role` + `RoleBinding`.
/// All operations use server-side apply (idempotent).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
#[tracing::instrument(skip(kube_client), fields(%ns_name), err)]
pub async fn ensure_session_namespace(
    kube_client: &kube::Client,
    ns_name: &str,
    session_id: &str,
    project_id: &str,
    platform_namespace: &str,
    gateway_namespace: &str,
    services_namespace: Option<&str>,
    dev_mode: bool,
) -> Result<(), super::error::DeployerError> {
    // 1. Namespace (pass services_namespace for correct NetworkPolicy)
    ensure_namespace_with_services_ns(
        kube_client,
        ns_name,
        "session",
        project_id,
        platform_namespace,
        gateway_namespace,
        services_namespace.unwrap_or(platform_namespace),
        dev_mode,
    )
    .await?;

    // 2. NetworkPolicy (unless dev mode) — session namespaces use a variant that
    //    allows ingress from the platform namespace on port 8000 for preview proxying.
    if !dev_mode {
        let svc_ns = services_namespace.unwrap_or(platform_namespace);
        let _ =
            ensure_session_network_policy(kube_client, ns_name, platform_namespace, svc_ns).await;
    }

    // 3. RBAC objects
    let (sa_json, role_json, rb_json) = build_session_rbac(ns_name);

    // Apply ServiceAccount
    apply_namespaced_object(
        kube_client,
        ns_name,
        "",
        "v1",
        "ServiceAccount",
        "serviceaccounts",
        "agent-sa",
        sa_json,
    )
    .await?;

    // Apply Role
    apply_namespaced_object(
        kube_client,
        ns_name,
        "rbac.authorization.k8s.io",
        "v1",
        "Role",
        "roles",
        "agent-edit",
        role_json,
    )
    .await?;

    // Apply RoleBinding
    apply_namespaced_object(
        kube_client,
        ns_name,
        "rbac.authorization.k8s.io",
        "v1",
        "RoleBinding",
        "rolebindings",
        "agent-edit-binding",
        rb_json,
    )
    .await?;

    // 4. ResourceQuota — prevent agents from creating unbounded pods/resources
    let quota_json = json!({
        "apiVersion": "v1",
        "kind": "ResourceQuota",
        "metadata": {
            "name": "session-quota",
            "namespace": ns_name
        },
        "spec": {
            "hard": {
                "pods": "10",
                "requests.cpu": "4",
                "requests.memory": "8Gi",
                "limits.cpu": "8",
                "limits.memory": "16Gi"
            }
        }
    });

    apply_namespaced_object(
        kube_client,
        ns_name,
        "",
        "v1",
        "ResourceQuota",
        "resourcequotas",
        "session-quota",
        quota_json,
    )
    .await?;

    // 5. LimitRange — ensure agent-created pods get default resource limits
    let limit_range_json = json!({
        "apiVersion": "v1",
        "kind": "LimitRange",
        "metadata": {
            "name": "session-limits",
            "namespace": ns_name
        },
        "spec": {
            "limits": [{
                "type": "Container",
                "default": {
                    "cpu": "1",
                    "memory": "1Gi"
                },
                "defaultRequest": {
                    "cpu": "100m",
                    "memory": "128Mi"
                }
            }]
        }
    });

    apply_namespaced_object(
        kube_client,
        ns_name,
        "",
        "v1",
        "LimitRange",
        "limitranges",
        "session-limits",
        limit_range_json,
    )
    .await?;

    tracing::info!(%ns_name, %session_id, "session namespace with RBAC ensured");
    Ok(())
}

/// Server-side apply a namespaced object.
#[allow(clippy::too_many_arguments)]
async fn apply_namespaced_object(
    kube_client: &kube::Client,
    ns_name: &str,
    group: &str,
    version: &str,
    kind: &str,
    plural: &str,
    name: &str,
    json_obj: serde_json::Value,
) -> Result<(), super::error::DeployerError> {
    let api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    let ar = kube::discovery::ApiResource {
        group: group.into(),
        version: version.into(),
        api_version,
        kind: kind.into(),
        plural: plural.into(),
    };
    let api: kube::Api<kube::api::DynamicObject> =
        kube::Api::namespaced_with(kube_client.clone(), ns_name, &ar);

    let obj: kube::api::DynamicObject = serde_json::from_value(json_obj)
        .map_err(|e| super::error::DeployerError::InvalidManifest(e.to_string()))?;

    let patch_params = kube::api::PatchParams::apply("platform-deployer").force();
    api.patch(name, &patch_params, &kube::api::Patch::Apply(&obj))
        .await?;

    Ok(())
}

/// Create or update the mesh CA trust bundle `ConfigMap` in the given namespace.
///
/// The `ConfigMap` contains the CA certificate PEM for mTLS verification.
/// Uses server-side apply (idempotent).
#[tracing::instrument(skip(kube_client, ca_pem), fields(%namespace), err)]
pub async fn ensure_mesh_ca_bundle(
    kube_client: &kube::Client,
    namespace: &str,
    ca_pem: &str,
) -> Result<(), super::error::DeployerError> {
    let cm_json = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "mesh-ca-bundle",
            "namespace": namespace,
            "labels": {
                "platform.io/managed-by": "platform"
            }
        },
        "data": {
            "ca.pem": ca_pem
        }
    });

    apply_namespaced_object(
        kube_client,
        namespace,
        "",
        "v1",
        "ConfigMap",
        "configmaps",
        "mesh-ca-bundle",
        cm_json,
    )
    .await
}

/// Delete a K8s namespace. Ignores 404 (already deleted).
///
/// S30: Refuses to delete namespaces not labelled `platform.io/managed-by: platform`.
pub async fn delete_namespace(kube: &kube::Client, ns_name: &str) -> Result<(), anyhow::Error> {
    let namespaces: kube::Api<k8s_openapi::api::core::v1::Namespace> = kube::Api::all(kube.clone());

    // S30: Verify the namespace is managed by us before deleting
    match namespaces.get(ns_name).await {
        Ok(ns) => {
            let labels = ns.metadata.labels.as_ref();
            let managed = labels
                .and_then(|l| l.get("platform.io/managed-by"))
                .is_some_and(|v| v == "platform");
            if !managed {
                anyhow::bail!(
                    "refusing to delete namespace '{ns_name}': missing platform.io/managed-by=platform label"
                );
            }
        }
        Err(kube::Error::Api(err)) if err.code == 404 => {
            tracing::debug!(namespace = %ns_name, "namespace already deleted");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }

    match namespaces
        .delete(ns_name, &kube::api::DeleteParams::default())
        .await
    {
        Ok(_) => {
            tracing::info!(namespace = %ns_name, "namespace deleted");
            Ok(())
        }
        Err(kube::Error::Api(err)) if err.code == 404 => {
            tracing::debug!(namespace = %ns_name, "namespace already deleted");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Convert a project name into a K8s-safe namespace slug.
///
/// - Lowercases
/// - Replaces non-alphanumeric chars with hyphens
/// - Collapses consecutive hyphens
/// - Strips leading/trailing hyphens
/// - Truncates to 40 chars (leaves room for `-dev`/`-prod` suffix, total ≤ 48)
///
/// Returns an error if the resulting slug would be empty (e.g. input is empty
/// or contains only non-alphanumeric characters).
pub fn slugify_namespace(name: &str) -> anyhow::Result<String> {
    let mut slug = String::with_capacity(name.len());
    let mut prev_hyphen = true; // suppress leading hyphens

    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen {
            slug.push('-');
            prev_hyphen = true;
        }
    }

    // Strip trailing hyphen
    if slug.ends_with('-') {
        slug.pop();
    }

    // Truncate to 40 chars at a clean boundary (no trailing hyphen)
    if slug.len() > 40 {
        slug.truncate(40);
        if slug.ends_with('-') {
            slug.pop();
        }
    }

    if slug.is_empty() {
        anyhow::bail!("namespace slug cannot be empty (input: {name:?})");
    }

    Ok(slug)
}

/// Build a Namespace JSON object for server-side apply.
///
/// `ns_name` is the full namespace name (e.g. `my-app-dev` or `prefix-my-app-dev`).
pub fn build_namespace_object(
    ns_name: &str,
    env: &str,
    project_id: &str,
    dev_mode: bool,
) -> serde_json::Value {
    let mut labels = serde_json::json!({
        "platform.io/project": project_id,
        "platform.io/env": env,
        "platform.io/managed-by": "platform"
    });

    // S3: PodSecurityAdmission — baseline enforcement on session namespaces
    // prevents agents from creating privileged pods, hostPath mounts, hostNetwork, etc.
    // Warn on restricted to surface what would break under stricter policy.
    if env == "session" && !dev_mode {
        let labels_obj = labels
            .as_object_mut()
            .expect("json!({}) always produces an Object");
        labels_obj.insert(
            "pod-security.kubernetes.io/enforce".into(),
            "baseline".into(),
        );
        labels_obj.insert(
            "pod-security.kubernetes.io/enforce-version".into(),
            "latest".into(),
        );
        labels_obj.insert(
            "pod-security.kubernetes.io/warn".into(),
            "restricted".into(),
        );
        labels_obj.insert(
            "pod-security.kubernetes.io/warn-version".into(),
            "latest".into(),
        );
    }

    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": ns_name,
            "labels": labels
        }
    })
}

/// Build a `NetworkPolicy` for a session namespace.
///
/// Build a namespace-level `NetworkPolicy` allowing egress to platform API (8080),
/// DNS, same-namespace, mTLS (8443) to platform-managed namespaces, and public
/// internet (blocking private ranges). Also allows ingress on 8443 from
/// platform-managed namespaces for mTLS.
fn build_namespace_network_policy(ns_name: &str, platform_namespace: &str) -> serde_json::Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": "platform-managed",
            "namespace": ns_name
        },
        "spec": {
            "podSelector": {},
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [
                // Allow all intra-namespace traffic (app ↔ db, test pod ↔ app, etc.)
                {
                    "from": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": ns_name
                            }
                        }
                    }]
                },
                // Allow mTLS ingress from other platform-managed namespaces
                {
                    "from": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "platform.io/managed-by": "platform"
                            }
                        }
                    }],
                    "ports": [{"port": 8443, "protocol": "TCP"}]
                }
            ],
            "egress": [
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": ns_name
                            }
                        }
                    }]
                },
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": platform_namespace
                            }
                        }
                    }],
                    "ports": [{"port": 8080, "protocol": "TCP"}]
                },
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": "kube-system"
                            }
                        },
                        "podSelector": {
                            "matchLabels": {
                                "k8s-app": "kube-dns"
                            }
                        }
                    }],
                    "ports": [
                        {"port": 53, "protocol": "UDP"},
                        {"port": 53, "protocol": "TCP"}
                    ]
                },
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "platform.io/managed-by": "platform"
                            }
                        }
                    }],
                    "ports": [{"port": 8443, "protocol": "TCP"}]
                },
                {
                    "to": [{
                        "ipBlock": {
                            "cidr": "0.0.0.0/0",
                            "except": [
                                "10.0.0.0/8",
                                "172.16.0.0/12",
                                "192.168.0.0/16",
                                "100.64.0.0/10",
                                "169.254.0.0/16"
                            ]
                        }
                    }]
                }
            ]
        }
    })
}

#[allow(clippy::too_many_lines)]
pub fn build_session_network_policy(
    ns_name: &str,
    platform_namespace: &str,
    services_namespace: &str,
) -> serde_json::Value {
    let mut egress_ns_selectors = vec![json!({
        "namespaceSelector": {
            "matchLabels": {
                "kubernetes.io/metadata.name": platform_namespace
            }
        }
    })];
    if services_namespace != platform_namespace {
        egress_ns_selectors.push(json!({
            "namespaceSelector": {
                "matchLabels": {
                    "kubernetes.io/metadata.name": services_namespace
                }
            }
        }));
    }

    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": "agent-isolation",
            "namespace": ns_name
        },
        "spec": {
            "podSelector": {
                "matchLabels": {
                    "platform.io/component": "agent-session"
                }
            },
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [
                {
                    "from": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": platform_namespace
                            }
                        }
                    }],
                    "ports": [{"port": 8000, "protocol": "TCP"}]
                },
                {
                    "from": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "platform.io/managed-by": "platform"
                            }
                        }
                    }],
                    "ports": [{"port": 8443, "protocol": "TCP"}]
                }
            ],
            "egress": [
                {
                    "to": egress_ns_selectors,
                    "ports": [
                        {"port": 8080, "protocol": "TCP"},
                        {"port": 6379, "protocol": "TCP"}
                    ]
                },
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": "kube-system"
                            }
                        },
                        "podSelector": {
                            "matchLabels": {
                                "k8s-app": "kube-dns"
                            }
                        }
                    }],
                    "ports": [
                        {"port": 53, "protocol": "UDP"},
                        {"port": 53, "protocol": "TCP"}
                    ]
                },
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "platform.io/managed-by": "platform"
                            }
                        }
                    }],
                    "ports": [{"port": 8443, "protocol": "TCP"}]
                },
                {
                    "to": [{
                        "ipBlock": {
                            "cidr": "0.0.0.0/0",
                            "except": [
                                "10.0.0.0/8",
                                "172.16.0.0/12",
                                "192.168.0.0/16",
                                "100.64.0.0/10",
                                "169.254.0.0/16"
                            ]
                        }
                    }]
                }
            ]
        }
    })
}

/// Build a `NetworkPolicy` JSON object for the `-dev` namespace.
///
/// Allows:
/// - Egress to the platform API namespace (port 8080)
/// - Egress to kube-system DNS (port 53 UDP+TCP)
/// - Egress to platform-managed namespaces on port 8443 (mTLS)
/// - Egress to internet (blocking cluster-internal CIDRs)
/// - Ingress from platform-managed namespaces on port 8443 (mTLS)
///
/// `ns_name` is the full namespace name (e.g. `my-app-dev` or `prefix-my-app-dev`).
pub fn build_network_policy(ns_name: &str, platform_namespace: &str) -> serde_json::Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": "agent-isolation",
            "namespace": ns_name
        },
        "spec": {
            "podSelector": {
                "matchLabels": {
                    "platform.io/component": "agent-session"
                }
            },
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [
                {
                    "from": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "platform.io/managed-by": "platform"
                            }
                        }
                    }],
                    "ports": [{"port": 8443, "protocol": "TCP"}]
                }
            ],
            "egress": [
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": platform_namespace
                            }
                        }
                    }],
                    "ports": [{"port": 8080, "protocol": "TCP"}]
                },
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "kubernetes.io/metadata.name": "kube-system"
                            }
                        },
                        "podSelector": {
                            "matchLabels": {
                                "k8s-app": "kube-dns"
                            }
                        }
                    }],
                    "ports": [
                        {"port": 53, "protocol": "UDP"},
                        {"port": 53, "protocol": "TCP"}
                    ]
                },
                {
                    "to": [{
                        "namespaceSelector": {
                            "matchLabels": {
                                "platform.io/managed-by": "platform"
                            }
                        }
                    }],
                    "ports": [{"port": 8443, "protocol": "TCP"}]
                },
                {
                    "to": [{
                        "ipBlock": {
                            "cidr": "0.0.0.0/0",
                            "except": [
                                "10.0.0.0/8",
                                "172.16.0.0/12",
                                "192.168.0.0/16",
                                "100.64.0.0/10",
                                "169.254.0.0/16"
                            ]
                        }
                    }]
                }
            ]
        }
    })
}

/// Ensure a K8s namespace exists using server-side apply (idempotent).
///
/// `ns_name` is the full namespace name (e.g. `my-app-dev` or `prefix-my-app-dev`).
/// `platform_namespace` is the namespace where the platform itself runs (for RBAC subjects).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[tracing::instrument(skip(kube_client), fields(%ns_name, %env), err)]
pub async fn ensure_namespace(
    kube_client: &kube::Client,
    ns_name: &str,
    env: &str,
    project_id: &str,
    platform_namespace: &str,
    gateway_namespace: &str,
    dev_mode: bool,
) -> Result<(), super::error::DeployerError> {
    ensure_namespace_inner(
        kube_client,
        ns_name,
        env,
        project_id,
        platform_namespace,
        gateway_namespace,
        None,
        dev_mode,
    )
    .await
}

/// Like `ensure_namespace` but allows specifying the services namespace
/// (where Valkey/Postgres/MinIO live) explicitly. In dev mode this differs
/// from `platform_namespace` because services run in `{ns_prefix}` not in
/// the platform's own namespace.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_namespace_with_services_ns(
    kube_client: &kube::Client,
    ns_name: &str,
    env: &str,
    project_id: &str,
    platform_namespace: &str,
    gateway_namespace: &str,
    services_namespace: &str,
    dev_mode: bool,
) -> Result<(), super::error::DeployerError> {
    ensure_namespace_inner(
        kube_client,
        ns_name,
        env,
        project_id,
        platform_namespace,
        gateway_namespace,
        Some(services_namespace),
        dev_mode,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn ensure_namespace_inner(
    kube_client: &kube::Client,
    ns_name: &str,
    env: &str,
    project_id: &str,
    platform_namespace: &str,
    gateway_namespace: &str,
    _services_namespace: Option<&str>,
    dev_mode: bool,
) -> Result<(), super::error::DeployerError> {
    let ns_json = build_namespace_object(ns_name, env, project_id, dev_mode);

    let ar = kube::discovery::ApiResource {
        group: String::new(),
        version: "v1".into(),
        api_version: "v1".into(),
        kind: "Namespace".into(),
        plural: "namespaces".into(),
    };
    let api: kube::Api<kube::api::DynamicObject> = kube::Api::all_with(kube_client.clone(), &ar);

    let obj: kube::api::DynamicObject = serde_json::from_value(ns_json)
        .map_err(|e| super::error::DeployerError::InvalidManifest(e.to_string()))?;

    let patch_params = kube::api::PatchParams::apply("platform-deployer").force();
    api.patch(ns_name, &patch_params, &kube::api::Patch::Apply(&obj))
        .await?;

    // S6: Create per-namespace RoleBinding for secrets access
    let secrets_rb = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {
            "name": "platform-secrets-access",
            "namespace": ns_name
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": format!("{platform_namespace}-secrets-manager")
        },
        "subjects": [{
            "kind": "ServiceAccount",
            "name": platform_namespace,
            "namespace": platform_namespace
        }]
    });

    apply_namespaced_object(
        kube_client,
        ns_name,
        "rbac.authorization.k8s.io",
        "v1",
        "RoleBinding",
        "rolebindings",
        "platform-secrets-access",
        secrets_rb,
    )
    .await?;

    // Create per-namespace RoleBinding for gateway access (HTTPRoutes + EndpointSlices)
    let gateway_rb = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {
            "name": "platform-gateway-access",
            "namespace": ns_name
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "platform-gateway"
        },
        "subjects": [{
            "kind": "ServiceAccount",
            "name": "platform-gateway",
            "namespace": gateway_namespace
        }]
    });

    apply_namespaced_object(
        kube_client,
        ns_name,
        "rbac.authorization.k8s.io",
        "v1",
        "RoleBinding",
        "rolebindings",
        "platform-gateway-access",
        gateway_rb,
    )
    .await?;

    let netpol = build_namespace_network_policy(ns_name, platform_namespace);

    apply_namespaced_object(
        kube_client,
        ns_name,
        "networking.k8s.io",
        "v1",
        "NetworkPolicy",
        "networkpolicies",
        "platform-managed",
        netpol,
    )
    .await?;

    tracing::info!(%ns_name, "namespace ensured");
    Ok(())
}

/// Ensure the session `NetworkPolicy` (with preview ingress) exists in the given namespace.
#[tracing::instrument(skip(kube_client), fields(%ns_name), err)]
pub async fn ensure_session_network_policy(
    kube_client: &kube::Client,
    ns_name: &str,
    platform_namespace: &str,
    services_namespace: &str,
) -> Result<(), super::error::DeployerError> {
    let np_json = build_session_network_policy(ns_name, platform_namespace, services_namespace);

    let ar = kube::discovery::ApiResource {
        group: "networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "networking.k8s.io/v1".into(),
        kind: "NetworkPolicy".into(),
        plural: "networkpolicies".into(),
    };
    let api: kube::Api<kube::api::DynamicObject> =
        kube::Api::namespaced_with(kube_client.clone(), ns_name, &ar);

    let obj: kube::api::DynamicObject = serde_json::from_value(np_json)
        .map_err(|e| super::error::DeployerError::InvalidManifest(e.to_string()))?;

    let patch_params = kube::api::PatchParams::apply("platform-deployer").force();
    api.patch(
        "agent-isolation",
        &patch_params,
        &kube::api::Patch::Apply(&obj),
    )
    .await?;

    tracing::info!(%ns_name, "session network policy ensured");
    Ok(())
}

/// Ensure the `NetworkPolicy` exists in the given namespace (idempotent).
///
/// `ns_name` is the full namespace name (e.g. `my-app-dev` or `prefix-my-app-dev`).
#[tracing::instrument(skip(kube_client), fields(%ns_name), err)]
pub async fn ensure_network_policy(
    kube_client: &kube::Client,
    ns_name: &str,
    platform_namespace: &str,
) -> Result<(), super::error::DeployerError> {
    let np_json = build_network_policy(ns_name, platform_namespace);

    let ar = kube::discovery::ApiResource {
        group: "networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "networking.k8s.io/v1".into(),
        kind: "NetworkPolicy".into(),
        plural: "networkpolicies".into(),
    };
    let api: kube::Api<kube::api::DynamicObject> =
        kube::Api::namespaced_with(kube_client.clone(), ns_name, &ar);

    let obj: kube::api::DynamicObject = serde_json::from_value(np_json)
        .map_err(|e| super::error::DeployerError::InvalidManifest(e.to_string()))?;

    let patch_params = kube::api::PatchParams::apply("platform-deployer").force();
    api.patch(
        "agent-isolation",
        &patch_params,
        &kube::api::Patch::Apply(&obj),
    )
    .await?;

    tracing::info!(%ns_name, "network policy ensured");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- slugify_namespace --

    #[test]
    fn slugify_namespace_basic() {
        assert_eq!(slugify_namespace("my-project").unwrap(), "my-project");
    }

    #[test]
    fn slugify_namespace_max_40_chars() {
        let long_name = "a".repeat(60);
        let slug = slugify_namespace(&long_name).unwrap();
        assert!(
            slug.len() <= 40,
            "slug should be ≤40 chars, got {}",
            slug.len()
        );
    }

    #[test]
    fn slugify_namespace_lowercase() {
        assert_eq!(slugify_namespace("My-Project").unwrap(), "my-project");
        assert_eq!(slugify_namespace("UPPER").unwrap(), "upper");
    }

    #[test]
    fn slugify_namespace_special_chars() {
        assert_eq!(slugify_namespace("my_project!v2").unwrap(), "my-project-v2");
        assert_eq!(slugify_namespace("hello  world").unwrap(), "hello-world");
    }

    #[test]
    fn slugify_namespace_leading_trailing_hyphens() {
        assert_eq!(slugify_namespace("--test--").unwrap(), "test");
        assert_eq!(slugify_namespace("___test___").unwrap(), "test");
    }

    #[test]
    fn slugify_namespace_empty() {
        assert!(slugify_namespace("").is_err());
    }

    #[test]
    fn slugify_namespace_all_special() {
        assert!(slugify_namespace("!!!").is_err());
    }

    #[test]
    fn slugify_namespace_truncation_no_trailing_hyphen() {
        // 42 chars where char 40 is a hyphen
        let name = format!("{}-{}", "a".repeat(39), "b".repeat(2));
        let slug = slugify_namespace(&name).unwrap();
        assert!(slug.len() <= 40);
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn slugify_namespace_unicode_replaced() {
        assert_eq!(slugify_namespace("café-app").unwrap(), "caf-app");
    }

    // -- build_namespace_object --

    #[test]
    fn namespace_object_has_correct_labels() {
        let ns = build_namespace_object("my-app-dev", "dev", "abc-123", false);
        let labels = ns["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/project"], "abc-123");
        assert_eq!(labels["platform.io/env"], "dev");
        assert_eq!(labels["platform.io/managed-by"], "platform");
        assert_eq!(ns["metadata"]["name"], "my-app-dev");
    }

    #[test]
    fn namespace_object_prod_env() {
        let ns = build_namespace_object("my-app-prod", "prod", "abc-123", false);
        assert_eq!(ns["metadata"]["name"], "my-app-prod");
        assert_eq!(ns["metadata"]["labels"]["platform.io/env"], "prod");
    }

    // -- build_network_policy --

    #[test]
    fn network_policy_egress_platform_api() {
        let np = build_network_policy("my-app", "platform");
        let egress = np["spec"]["egress"].as_array().unwrap();
        // First rule: platform API
        let platform_rule = &egress[0];
        let ns_selector = &platform_rule["to"][0]["namespaceSelector"]["matchLabels"];
        assert_eq!(ns_selector["kubernetes.io/metadata.name"], "platform");
        let ports = platform_rule["ports"].as_array().unwrap();
        assert_eq!(ports[0]["port"], 8080);
    }

    #[test]
    fn network_policy_egress_dns_kube_system() {
        let np = build_network_policy("my-app", "platform");
        let egress = np["spec"]["egress"].as_array().unwrap();
        // Second rule: DNS
        let dns_rule = &egress[1];
        let ns_selector = &dns_rule["to"][0]["namespaceSelector"]["matchLabels"];
        assert_eq!(ns_selector["kubernetes.io/metadata.name"], "kube-system");
        let pod_selector = &dns_rule["to"][0]["podSelector"]["matchLabels"];
        assert_eq!(pod_selector["k8s-app"], "kube-dns");
        let ports = dns_rule["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0]["port"], 53);
        assert_eq!(ports[0]["protocol"], "UDP");
        assert_eq!(ports[1]["port"], 53);
        assert_eq!(ports[1]["protocol"], "TCP");
    }

    #[test]
    fn network_policy_egress_internet_except_cluster() {
        let np = build_network_policy("my-app", "platform");
        let egress = np["spec"]["egress"].as_array().unwrap();
        // Fourth rule: internet (after platform API, DNS, mTLS)
        let internet_rule = &egress[3];
        let ip_block = &internet_rule["to"][0]["ipBlock"];
        assert_eq!(ip_block["cidr"], "0.0.0.0/0");
        let except = ip_block["except"].as_array().unwrap();
        let except_strs: Vec<&str> = except.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(except_strs.contains(&"10.0.0.0/8"));
        assert!(except_strs.contains(&"172.16.0.0/12"));
        assert!(except_strs.contains(&"192.168.0.0/16"));
        assert!(except_strs.contains(&"100.64.0.0/10"));
        assert!(except_strs.contains(&"169.254.0.0/16"));
    }

    #[test]
    fn network_policy_ingress_allows_mtls() {
        let np = build_network_policy("my-app", "platform");
        let policy_types = np["spec"]["policyTypes"].as_array().unwrap();
        let types: Vec<&str> = policy_types.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(types.contains(&"Ingress"));
        assert!(types.contains(&"Egress"));
        // Ingress allows mTLS on 8443 from platform-managed namespaces
        let ingress = np["spec"]["ingress"].as_array().unwrap();
        assert_eq!(ingress.len(), 1);
        let mtls_rule = &ingress[0];
        let from_selector =
            &mtls_rule["from"][0]["namespaceSelector"]["matchLabels"]["platform.io/managed-by"];
        assert_eq!(from_selector, "platform");
        let ports = mtls_rule["ports"].as_array().unwrap();
        assert_eq!(ports[0]["port"], 8443);
    }

    #[test]
    fn network_policy_egress_mtls() {
        let np = build_network_policy("my-app", "platform");
        let egress = np["spec"]["egress"].as_array().unwrap();
        // Third rule: mTLS egress to platform-managed namespaces
        let mtls_rule = &egress[2];
        let to_selector =
            &mtls_rule["to"][0]["namespaceSelector"]["matchLabels"]["platform.io/managed-by"];
        assert_eq!(to_selector, "platform");
        let ports = mtls_rule["ports"].as_array().unwrap();
        assert_eq!(ports[0]["port"], 8443);
    }

    #[test]
    fn network_policy_pod_selector() {
        let np = build_network_policy("my-app", "platform");
        let selector = &np["spec"]["podSelector"]["matchLabels"];
        assert_eq!(selector["platform.io/component"], "agent-session");
    }

    #[test]
    fn network_policy_namespace_is_dev() {
        let np = build_network_policy("my-app-dev", "platform");
        assert_eq!(np["metadata"]["namespace"], "my-app-dev");
    }

    // -- session_namespace_name --

    #[test]
    fn session_namespace_name_basic() {
        let config = Config::test_default();
        assert_eq!(
            session_namespace_name(&config, "myapp", "abc12345"),
            "myapp-s-abc12345"
        );
    }

    #[test]
    fn session_namespace_name_with_prefix() {
        let mut config = Config::test_default();
        config.ns_prefix = Some("test".into());
        assert_eq!(
            session_namespace_name(&config, "myapp", "abc12345"),
            "test-myapp-s-abc12345"
        );
    }

    #[test]
    fn session_namespace_name_long_slug() {
        let config = Config::test_default();
        let slug = "a".repeat(40);
        let name = session_namespace_name(&config, &slug, "abc12345");
        assert!(
            name.len() <= 63,
            "session namespace should fit DNS label limit, got {} chars",
            name.len()
        );
    }

    // -- build_session_rbac --

    // -- build_session_network_policy --

    #[test]
    fn session_network_policy_ingress_allows_platform_and_mtls() {
        let np = build_session_network_policy("my-app", "platform", "platform");
        let ingress = np["spec"]["ingress"].as_array().unwrap();
        assert_eq!(
            ingress.len(),
            2,
            "should have preview (8000) + mTLS (8443) ingress"
        );
        // First rule: preview proxy from platform namespace
        let preview_rule = &ingress[0];
        let ns_selector = &preview_rule["from"][0]["namespaceSelector"]["matchLabels"];
        assert_eq!(ns_selector["kubernetes.io/metadata.name"], "platform");
        let ports = preview_rule["ports"].as_array().unwrap();
        assert_eq!(ports[0]["port"], 8000);
        // Second rule: mTLS from platform-managed namespaces
        let mtls_rule = &ingress[1];
        let mtls_selector =
            &mtls_rule["from"][0]["namespaceSelector"]["matchLabels"]["platform.io/managed-by"];
        assert_eq!(mtls_selector, "platform");
        let mtls_ports = mtls_rule["ports"].as_array().unwrap();
        assert_eq!(mtls_ports[0]["port"], 8443);
    }

    #[test]
    fn session_network_policy_egress_includes_valkey() {
        let session_np = build_session_network_policy("my-app", "platform", "platform");
        let egress = session_np["spec"]["egress"].as_array().unwrap();
        // First rule: platform API (8080) + Valkey (6379)
        let platform_rule = &egress[0];
        let ports = platform_rule["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0]["port"], 8080);
        assert_eq!(ports[1]["port"], 6379);
    }

    #[test]
    fn session_network_policy_egress_includes_mtls() {
        let session_np = build_session_network_policy("my-app", "platform", "platform");
        let egress = session_np["spec"]["egress"].as_array().unwrap();
        // Third rule (index 2): mTLS to platform-managed namespaces
        let mtls_rule = &egress[2];
        let to_selector =
            &mtls_rule["to"][0]["namespaceSelector"]["matchLabels"]["platform.io/managed-by"];
        assert_eq!(to_selector, "platform");
        let ports = mtls_rule["ports"].as_array().unwrap();
        assert_eq!(ports[0]["port"], 8443);
    }

    #[test]
    fn project_network_policy_allows_mtls_ingress_only() {
        // Verify build_network_policy allows mTLS ingress on 8443
        let np = build_network_policy("my-app", "platform");
        let ingress = np["spec"]["ingress"].as_array().unwrap();
        assert_eq!(ingress.len(), 1, "only mTLS ingress should be allowed");
        assert_eq!(ingress[0]["ports"][0]["port"], 8443);
    }

    // -- build_session_rbac --

    #[test]
    fn build_session_rbac_service_account() {
        let (sa, _, _) = build_session_rbac("test-ns");
        assert_eq!(sa["metadata"]["name"], "agent-sa");
        assert_eq!(sa["metadata"]["namespace"], "test-ns");
        assert_eq!(sa["kind"], "ServiceAccount");
    }

    #[test]
    fn build_session_rbac_role_includes_core_resources() {
        let (_, role, _) = build_session_rbac("test-ns");
        assert_eq!(role["metadata"]["name"], "agent-edit");
        let rules = role["rules"].as_array().unwrap();
        let core_rule = &rules[0];
        assert_eq!(core_rule["apiGroups"][0], "");
        let resources: Vec<&str> = core_rule["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(resources.contains(&"pods"));
        assert!(resources.contains(&"services"));
        assert!(resources.contains(&"configmaps"));
        assert!(resources.contains(&"secrets"));
        assert!(resources.contains(&"pods/log"));
        assert!(resources.contains(&"pods/exec"));
    }

    #[test]
    fn build_session_rbac_role_includes_apps() {
        let (_, role, _) = build_session_rbac("test-ns");
        let rules = role["rules"].as_array().unwrap();
        let apps_rule = &rules[1];
        assert_eq!(apps_rule["apiGroups"][0], "apps");
        let resources: Vec<&str> = apps_rule["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(resources.contains(&"deployments"));
        assert!(resources.contains(&"statefulsets"));
        assert!(resources.contains(&"daemonsets"));
        assert!(resources.contains(&"replicasets"));
    }

    #[test]
    fn build_session_rbac_role_includes_batch() {
        let (_, role, _) = build_session_rbac("test-ns");
        let rules = role["rules"].as_array().unwrap();
        let batch_rule = &rules[2];
        assert_eq!(batch_rule["apiGroups"][0], "batch");
        let resources: Vec<&str> = batch_rule["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(resources.contains(&"jobs"));
        assert!(resources.contains(&"cronjobs"));
    }

    #[test]
    fn build_session_rbac_role_excludes_networkpolicies() {
        let (_, role, _) = build_session_rbac("test-ns");
        let rules = role["rules"].as_array().unwrap();
        for rule in rules {
            let groups: Vec<&str> = rule["apiGroups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(
                !groups.contains(&"networking.k8s.io"),
                "role should not include networking.k8s.io API group"
            );
        }
    }

    #[test]
    fn build_session_rbac_rolebinding_links_sa_to_role() {
        let (_, _, rb) = build_session_rbac("test-ns");
        assert_eq!(rb["metadata"]["name"], "agent-edit-binding");
        assert_eq!(rb["roleRef"]["name"], "agent-edit");
        assert_eq!(rb["roleRef"]["kind"], "Role");
        let subjects = rb["subjects"].as_array().unwrap();
        assert_eq!(subjects[0]["name"], "agent-sa");
        assert_eq!(subjects[0]["kind"], "ServiceAccount");
        assert_eq!(subjects[0]["namespace"], "test-ns");
    }

    // -- session namespace name DNS label limit --

    #[test]
    fn session_namespace_name_always_under_63_chars() {
        let config = Config::test_default();
        // 40-char slug (max from slugify_namespace) + "-s-" + 8-char ID = 51 chars
        let slug = "a".repeat(40);
        let short_id = "12345678";
        let name = session_namespace_name(&config, &slug, short_id);
        assert!(
            name.len() <= 63,
            "session namespace name must be <= 63 chars (DNS label limit), got {} chars: {name}",
            name.len()
        );
    }

    #[test]
    fn session_namespace_name_with_prefix_under_63_chars() {
        let mut config = Config::test_default();
        // Short prefix
        config.ns_prefix = Some("pf".into());
        let slug = "a".repeat(40);
        let short_id = "12345678";
        let name = session_namespace_name(&config, &slug, short_id);
        // pf- + 40 + -s- + 8 = 54 chars
        assert!(
            name.len() <= 63,
            "with prefix, session namespace should be <= 63 chars, got {} chars: {name}",
            name.len()
        );
    }

    // -- build_namespace_object: pod security admission for session --

    #[test]
    fn namespace_object_session_has_pod_security_labels() {
        let ns = build_namespace_object("myapp-s-abc123", "session", "proj-1", false);
        let labels = ns["metadata"]["labels"].as_object().unwrap();
        assert_eq!(
            labels.get("pod-security.kubernetes.io/enforce"),
            Some(&serde_json::Value::String("baseline".into())),
            "session namespace should enforce baseline PSA"
        );
        assert_eq!(
            labels.get("pod-security.kubernetes.io/enforce-version"),
            Some(&serde_json::Value::String("latest".into())),
        );
        assert_eq!(
            labels.get("pod-security.kubernetes.io/warn"),
            Some(&serde_json::Value::String("restricted".into())),
            "session namespace should warn on restricted PSA"
        );
        assert_eq!(
            labels.get("pod-security.kubernetes.io/warn-version"),
            Some(&serde_json::Value::String("latest".into())),
        );
    }

    #[test]
    fn namespace_object_session_dev_mode_skips_psa() {
        let ns = build_namespace_object("myapp-s-abc123", "session", "proj-1", true);
        let labels = ns["metadata"]["labels"].as_object().unwrap();
        assert!(
            labels.get("pod-security.kubernetes.io/enforce").is_none(),
            "dev mode session should not have PSA labels"
        );
        assert!(
            labels.get("pod-security.kubernetes.io/warn").is_none(),
            "dev mode session should not have PSA warn labels"
        );
    }

    #[test]
    fn namespace_object_non_session_env_no_psa() {
        let ns = build_namespace_object("myapp-dev", "dev", "proj-1", false);
        let labels = ns["metadata"]["labels"].as_object().unwrap();
        assert!(
            labels.get("pod-security.kubernetes.io/enforce").is_none(),
            "non-session env should not have PSA labels"
        );
    }

    // -- build_session_network_policy with different services_namespace --

    #[test]
    fn session_network_policy_different_services_namespace() {
        let np = build_session_network_policy("my-app", "platform", "services-ns");
        let egress = np["spec"]["egress"].as_array().unwrap();
        // First egress rule should have 2 namespace selectors (platform + services-ns)
        let platform_rule = &egress[0];
        let to = platform_rule["to"].as_array().unwrap();
        assert_eq!(
            to.len(),
            2,
            "should have two namespace selectors when services_namespace differs"
        );
        let ns1 = &to[0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"];
        let ns2 = &to[1]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"];
        assert_eq!(ns1, "platform");
        assert_eq!(ns2, "services-ns");
    }

    #[test]
    fn session_network_policy_same_services_namespace_deduplicates() {
        let np = build_session_network_policy("my-app", "platform", "platform");
        let egress = np["spec"]["egress"].as_array().unwrap();
        let platform_rule = &egress[0];
        let to = platform_rule["to"].as_array().unwrap();
        assert_eq!(
            to.len(),
            1,
            "should have one selector when services_namespace == platform_namespace"
        );
    }

    // -- build_namespace_network_policy --

    #[test]
    fn namespace_network_policy_has_five_egress_rules() {
        let np = build_namespace_network_policy("my-app", "platform");
        let egress = np["spec"]["egress"].as_array().unwrap();
        // 5 rules: same-namespace + platform API + DNS + mTLS + public internet
        assert_eq!(
            egress.len(),
            5,
            "expected 5 egress rules (same-ns, platform, dns, mTLS, internet): got {}",
            egress.len()
        );
    }

    #[test]
    fn namespace_network_policy_has_ingress_rules() {
        let np = build_namespace_network_policy("my-app", "platform");
        let ingress = np["spec"]["ingress"].as_array().unwrap();
        assert_eq!(ingress.len(), 2, "expected 2 ingress rules (same-ns, mTLS)");

        // First rule: same-namespace (all ports)
        let same_ns_rule = &ingress[0];
        let from_selector = &same_ns_rule["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"];
        assert_eq!(from_selector, "my-app");
        assert!(
            same_ns_rule["ports"].is_null(),
            "same-ns rule allows all ports"
        );

        // Second rule: mTLS from platform-managed namespaces (port 8443)
        let mtls_rule = &ingress[1];
        let from_selector =
            &mtls_rule["from"][0]["namespaceSelector"]["matchLabels"]["platform.io/managed-by"];
        assert_eq!(from_selector, "platform");
        let ports = mtls_rule["ports"].as_array().unwrap();
        assert_eq!(ports[0]["port"], 8443);
    }

    #[test]
    fn namespace_network_policy_has_mtls_egress() {
        let np = build_namespace_network_policy("my-app", "platform");
        let egress = np["spec"]["egress"].as_array().unwrap();
        // Fourth rule (index 3): mTLS egress
        let mtls_rule = &egress[3];
        let to_selector =
            &mtls_rule["to"][0]["namespaceSelector"]["matchLabels"]["platform.io/managed-by"];
        assert_eq!(to_selector, "platform");
        let ports = mtls_rule["ports"].as_array().unwrap();
        assert_eq!(ports[0]["port"], 8443);
    }

    // -- build_session_rbac: all 3 rules, no networking.k8s.io --

    #[test]
    fn build_session_rbac_has_exactly_3_rules() {
        let (_, role, _) = build_session_rbac("test-ns");
        let rules = role["rules"].as_array().unwrap();
        assert_eq!(
            rules.len(),
            3,
            "should have core, apps, batch (no networking.k8s.io)"
        );
    }

    #[test]
    fn build_session_rbac_role_all_verbs_wildcard() {
        let (_, role, _) = build_session_rbac("test-ns");
        let rules = role["rules"].as_array().unwrap();
        for rule in rules {
            let verbs = rule["verbs"].as_array().unwrap();
            assert_eq!(verbs.len(), 1);
            assert_eq!(verbs[0], "*");
        }
    }

    // -- build_network_policy: agent-session selector --

    #[test]
    fn build_network_policy_targets_agent_session_pods() {
        let np = build_network_policy("my-ns", "platform");
        let selector = &np["spec"]["podSelector"]["matchLabels"]["platform.io/component"];
        assert_eq!(selector, "agent-session");
    }

    // -- namespace_object general --

    #[test]
    fn namespace_object_has_managed_by_label() {
        let ns = build_namespace_object("test-ns", "dev", "proj-123", false);
        let labels = ns["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/managed-by"], "platform");
    }

    #[test]
    fn namespace_object_kind_and_api_version() {
        let ns = build_namespace_object("test-ns", "dev", "proj-123", false);
        assert_eq!(ns["apiVersion"], "v1");
        assert_eq!(ns["kind"], "Namespace");
    }

    // -- session_namespace_name: format variants --

    #[test]
    fn session_namespace_name_format_no_prefix() {
        let config = Config::test_default();
        let name = session_namespace_name(&config, "my-app", "abcd1234");
        assert_eq!(name, "my-app-s-abcd1234");
    }

    #[test]
    fn session_namespace_name_format_with_prefix() {
        let mut config = Config::test_default();
        config.ns_prefix = Some("prod".into());
        let name = session_namespace_name(&config, "my-app", "abcd1234");
        assert_eq!(name, "prod-my-app-s-abcd1234");
    }

    // -- slugify_namespace: additional edge cases --

    #[test]
    fn slugify_namespace_single_char() {
        assert_eq!(slugify_namespace("a").unwrap(), "a");
    }

    #[test]
    fn slugify_namespace_numeric_only() {
        assert_eq!(slugify_namespace("12345").unwrap(), "12345");
    }

    #[test]
    fn slugify_namespace_mixed_special_at_truncation_boundary() {
        // Create a name that would have a hyphen right at char 40 after conversion
        let mut name = "a".repeat(39);
        name.push('-');
        name.push('b');
        let slug = slugify_namespace(&name).unwrap();
        assert!(slug.len() <= 40);
        assert!(!slug.ends_with('-'));
    }
}
