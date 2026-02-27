use serde_json::json;

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
pub fn build_namespace_object(
    namespace_slug: &str,
    env: &str,
    project_id: &str,
) -> serde_json::Value {
    let ns_name = format!("{namespace_slug}-{env}");
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

/// Build a `NetworkPolicy` JSON object for the `-dev` namespace.
///
/// Allows:
/// - Egress to the platform API namespace (port 8080)
/// - Egress to kube-system DNS (port 53 UDP+TCP)
/// - Egress to internet (blocking cluster-internal CIDRs)
/// - Ingress: deny all (no ingress rules)
pub fn build_network_policy(namespace_slug: &str, platform_namespace: &str) -> serde_json::Value {
    let ns_name = format!("{namespace_slug}-dev");
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
#[tracing::instrument(skip(kube_client), fields(%namespace_slug, %env), err)]
pub async fn ensure_namespace(
    kube_client: &kube::Client,
    namespace_slug: &str,
    env: &str,
    project_id: &str,
) -> Result<(), super::error::DeployerError> {
    let ns_json = build_namespace_object(namespace_slug, env, project_id);
    let ns_name = format!("{namespace_slug}-{env}");

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
    api.patch(&ns_name, &patch_params, &kube::api::Patch::Apply(&obj))
        .await?;

    tracing::info!(%ns_name, "namespace ensured");
    Ok(())
}

/// Ensure the `NetworkPolicy` exists in the `-dev` namespace (idempotent).
#[tracing::instrument(skip(kube_client), fields(%namespace_slug), err)]
pub async fn ensure_network_policy(
    kube_client: &kube::Client,
    namespace_slug: &str,
    platform_namespace: &str,
) -> Result<(), super::error::DeployerError> {
    let np_json = build_network_policy(namespace_slug, platform_namespace);
    let ns_name = format!("{namespace_slug}-dev");

    let ar = kube::discovery::ApiResource {
        group: "networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "networking.k8s.io/v1".into(),
        kind: "NetworkPolicy".into(),
        plural: "networkpolicies".into(),
    };
    let api: kube::Api<kube::api::DynamicObject> =
        kube::Api::namespaced_with(kube_client.clone(), &ns_name, &ar);

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
        let ns = build_namespace_object("my-app", "dev", "abc-123");
        let labels = ns["metadata"]["labels"].as_object().unwrap();
        assert_eq!(labels["platform.io/project"], "abc-123");
        assert_eq!(labels["platform.io/env"], "dev");
        assert_eq!(labels["platform.io/managed-by"], "platform");
        assert_eq!(ns["metadata"]["name"], "my-app-dev");
    }

    #[test]
    fn namespace_object_prod_env() {
        let ns = build_namespace_object("my-app", "prod", "abc-123");
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
        let np = build_network_policy("my-app", "platform");
        assert_eq!(np["metadata"]["namespace"], "my-app-dev");
    }
}
