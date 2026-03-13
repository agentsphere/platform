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
#[tracing::instrument(skip(kube_client), fields(%ns_name), err)]
pub async fn ensure_session_namespace(
    kube_client: &kube::Client,
    ns_name: &str,
    session_id: &str,
    project_id: &str,
    platform_namespace: &str,
    dev_mode: bool,
) -> Result<(), super::error::DeployerError> {
    // 1. Namespace
    ensure_namespace(kube_client, ns_name, "session", project_id).await?;

    // 2. NetworkPolicy (unless dev mode) — session namespaces use a variant that
    //    allows ingress from the platform namespace on port 8000 for preview proxying.
    if !dev_mode {
        let _ = ensure_session_network_policy(kube_client, ns_name, platform_namespace).await;
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

/// Delete a K8s namespace. Ignores 404 (already deleted).
pub async fn delete_namespace(kube: &kube::Client, ns_name: &str) -> Result<(), anyhow::Error> {
    let namespaces: kube::Api<k8s_openapi::api::core::v1::Namespace> = kube::Api::all(kube.clone());
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
pub fn slugify_namespace(name: &str) -> String {
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

    slug
}

/// Build a Namespace JSON object for server-side apply.
///
/// `ns_name` is the full namespace name (e.g. `my-app-dev` or `prefix-my-app-dev`).
pub fn build_namespace_object(ns_name: &str, env: &str, project_id: &str) -> serde_json::Value {
    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": ns_name,
            "labels": {
                "platform.io/project": project_id,
                "platform.io/env": env,
                "platform.io/managed-by": "platform"
            }
        }
    })
}

/// Build a `NetworkPolicy` for a session namespace.
///
/// Same egress rules as `build_network_policy()` (platform API, DNS, internet) but
/// additionally allows ingress from the platform namespace on port 8000 TCP for
/// iframe preview proxying.
pub fn build_session_network_policy(ns_name: &str, platform_namespace: &str) -> serde_json::Value {
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
            "ingress": [{
                "from": [{
                    "namespaceSelector": {
                        "matchLabels": {
                            "kubernetes.io/metadata.name": platform_namespace
                        }
                    }
                }],
                "ports": [{"port": 8000, "protocol": "TCP"}]
            }],
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
/// - Egress to internet (blocking cluster-internal CIDRs)
/// - Ingress: deny all (no ingress rules)
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
#[tracing::instrument(skip(kube_client), fields(%ns_name, %env), err)]
pub async fn ensure_namespace(
    kube_client: &kube::Client,
    ns_name: &str,
    env: &str,
    project_id: &str,
) -> Result<(), super::error::DeployerError> {
    let ns_json = build_namespace_object(ns_name, env, project_id);

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

    tracing::info!(%ns_name, "namespace ensured");
    Ok(())
}

/// Ensure the session `NetworkPolicy` (with preview ingress) exists in the given namespace.
#[tracing::instrument(skip(kube_client), fields(%ns_name), err)]
pub async fn ensure_session_network_policy(
    kube_client: &kube::Client,
    ns_name: &str,
    platform_namespace: &str,
) -> Result<(), super::error::DeployerError> {
    let np_json = build_session_network_policy(ns_name, platform_namespace);

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
        assert_eq!(slugify_namespace("my-project"), "my-project");
    }

    #[test]
    fn slugify_namespace_max_40_chars() {
        let long_name = "a".repeat(60);
        let slug = slugify_namespace(&long_name);
        assert!(
            slug.len() <= 40,
            "slug should be ≤40 chars, got {}",
            slug.len()
        );
    }

    #[test]
    fn slugify_namespace_lowercase() {
        assert_eq!(slugify_namespace("My-Project"), "my-project");
        assert_eq!(slugify_namespace("UPPER"), "upper");
    }

    #[test]
    fn slugify_namespace_special_chars() {
        assert_eq!(slugify_namespace("my_project!v2"), "my-project-v2");
        assert_eq!(slugify_namespace("hello  world"), "hello-world");
    }

    #[test]
    fn slugify_namespace_leading_trailing_hyphens() {
        assert_eq!(slugify_namespace("--test--"), "test");
        assert_eq!(slugify_namespace("___test___"), "test");
    }

    #[test]
    fn slugify_namespace_empty() {
        assert_eq!(slugify_namespace(""), "");
    }

    #[test]
    fn slugify_namespace_all_special() {
        assert_eq!(slugify_namespace("!!!"), "");
    }

    #[test]
    fn slugify_namespace_truncation_no_trailing_hyphen() {
        // 42 chars where char 40 is a hyphen
        let name = format!("{}-{}", "a".repeat(39), "b".repeat(2));
        let slug = slugify_namespace(&name);
        assert!(slug.len() <= 40);
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn slugify_namespace_unicode_replaced() {
        assert_eq!(slugify_namespace("café-app"), "caf-app");
    }

    // -- build_namespace_object --

    #[test]
    fn namespace_object_has_correct_labels() {
        let ns = build_namespace_object("my-app-dev", "dev", "abc-123");
        let labels = ns["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/project"], "abc-123");
        assert_eq!(labels["platform.io/env"], "dev");
        assert_eq!(labels["platform.io/managed-by"], "platform");
        assert_eq!(ns["metadata"]["name"], "my-app-dev");
    }

    #[test]
    fn namespace_object_prod_env() {
        let ns = build_namespace_object("my-app-prod", "prod", "abc-123");
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
        // Third rule: internet
        let internet_rule = &egress[2];
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
    fn network_policy_ingress_deny_all() {
        let np = build_network_policy("my-app", "platform");
        let policy_types = np["spec"]["policyTypes"].as_array().unwrap();
        let types: Vec<&str> = policy_types.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(types.contains(&"Ingress"));
        assert!(types.contains(&"Egress"));
        // No ingress rules = deny all ingress
        assert!(np["spec"]["ingress"].is_null());
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
    fn session_network_policy_ingress_allows_platform() {
        let np = build_session_network_policy("my-app", "platform");
        let ingress = np["spec"]["ingress"].as_array().unwrap();
        assert_eq!(ingress.len(), 1);
        let rule = &ingress[0];
        let ns_selector = &rule["from"][0]["namespaceSelector"]["matchLabels"];
        assert_eq!(ns_selector["kubernetes.io/metadata.name"], "platform");
        let ports = rule["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0]["port"], 8000);
        assert_eq!(ports[0]["protocol"], "TCP");
    }

    #[test]
    fn session_network_policy_egress_unchanged() {
        let session_np = build_session_network_policy("my-app", "platform");
        let project_np = build_network_policy("my-app", "platform");
        assert_eq!(session_np["spec"]["egress"], project_np["spec"]["egress"]);
    }

    #[test]
    fn project_network_policy_still_denies_ingress() {
        // Verify build_network_policy hasn't been accidentally modified
        let np = build_network_policy("my-app", "platform");
        assert!(np["spec"]["ingress"].is_null());
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
}
