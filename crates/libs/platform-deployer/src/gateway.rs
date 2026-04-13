// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Gateway API (`HTTPRoute`) builders for traffic splitting.
//!
//! Platform creates/manages `HTTPRoute` resources referencing a shared Gateway
//! (created at cluster setup time) via cross-namespace `parentRefs`.
//! Uses kube-rs `DynamicObject` — no new crate dependency needed.

use serde_json::json;

use crate::error::DeployerError;

/// Shared Gateway reference used by all `HTTPRoute` builders.
pub struct GatewayRef<'a> {
    pub name: &'a str,
    pub namespace: &'a str,
}

/// Build an `HTTPRoute` JSON for weighted traffic splitting between stable and canary services.
///
/// The route sends `stable_weight`% to `stable_service` and `canary_weight`% to `canary_service`.
/// References a shared Gateway via cross-namespace `parentRefs`.
///
/// Returns an error if `stable_weight + canary_weight != 100`.
#[allow(clippy::too_many_arguments)]
pub fn build_weighted_httproute(
    name: &str,
    namespace: &str,
    hostname: &str,
    stable_service: &str,
    canary_service: &str,
    stable_weight: u32,
    canary_weight: u32,
    gw: &GatewayRef<'_>,
) -> Result<serde_json::Value, DeployerError> {
    if stable_weight + canary_weight != 100 {
        return Err(DeployerError::GatewayError(format!(
            "traffic weights must sum to 100, got {}",
            stable_weight + canary_weight
        )));
    }

    let mut spec = serde_json::json!({
        "parentRefs": [{
            "name": gw.name,
            "namespace": gw.namespace,
        }],
        "rules": [{
            "backendRefs": [
                {
                    "name": stable_service,
                    "port": 8080,
                    "weight": stable_weight,
                },
                {
                    "name": canary_service,
                    "port": 8080,
                    "weight": canary_weight,
                },
            ]
        }]
    });
    // Only add hostnames if it's a real hostname (not wildcard "*")
    if hostname != "*" {
        spec["hostnames"] = json!([hostname]);
    }

    Ok(json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": {
                "platform.io/managed-by": "platform-deployer"
            }
        },
        "spec": spec
    }))
}

/// Build an `HTTPRoute` JSON for header-based routing (A/B testing).
///
/// Requests matching the specified header are routed to the treatment service;
/// all other requests go to the control service.
/// References a shared Gateway via cross-namespace `parentRefs`.
pub fn build_header_match_httproute<S: std::hash::BuildHasher>(
    name: &str,
    namespace: &str,
    hostname: &str,
    control_service: &str,
    treatment_service: &str,
    headers: &std::collections::HashMap<String, String, S>,
    gw: &GatewayRef<'_>,
) -> serde_json::Value {
    let header_matches: Vec<serde_json::Value> = headers
        .iter()
        .map(|(k, v)| {
            json!({
                "type": "Exact",
                "name": k,
                "value": v,
            })
        })
        .collect();

    let mut spec = serde_json::json!({
        "parentRefs": [{
            "name": gw.name,
            "namespace": gw.namespace,
        }],
        "rules": [
            {
                "matches": [{
                    "headers": header_matches,
                }],
                "backendRefs": [{
                    "name": treatment_service,
                    "port": 8080,
                }]
            },
            {
                "backendRefs": [{
                    "name": control_service,
                    "port": 8080,
                }]
            }
        ]
    });
    if hostname != "*" {
        spec["hostnames"] = json!([hostname]);
    }

    json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": {
                "platform.io/managed-by": "platform-deployer"
            }
        },
        "spec": spec
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_httproute_structure() {
        let route = build_weighted_httproute(
            "api-canary",
            "myapp-prod",
            "api.example.com",
            "api-stable",
            "api-canary",
            80,
            20,
            &GatewayRef {
                name: "platform-gateway",
                namespace: "envoy-gateway-system",
            },
        )
        .unwrap();

        assert_eq!(route["apiVersion"], "gateway.networking.k8s.io/v1");
        assert_eq!(route["kind"], "HTTPRoute");
        assert_eq!(route["metadata"]["name"], "api-canary");
        assert_eq!(route["metadata"]["namespace"], "myapp-prod");

        // parentRefs should reference the shared gateway cross-namespace
        let parent_refs = route["spec"]["parentRefs"].as_array().unwrap();
        assert_eq!(parent_refs.len(), 1);
        assert_eq!(parent_refs[0]["name"], "platform-gateway");
        assert_eq!(parent_refs[0]["namespace"], "envoy-gateway-system");

        let rules = route["spec"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        let backends = rules[0]["backendRefs"].as_array().unwrap();
        assert_eq!(backends.len(), 2);
        assert_eq!(backends[0]["name"], "api-stable");
        assert_eq!(backends[0]["port"], 8080);
        assert_eq!(backends[0]["weight"], 80);
        assert_eq!(backends[1]["name"], "api-canary");
        assert_eq!(backends[1]["port"], 8080);
        assert_eq!(backends[1]["weight"], 20);
    }

    #[test]
    fn weighted_httproute_100_percent_stable() {
        let route = build_weighted_httproute(
            "api-route",
            "ns",
            "api.example.com",
            "stable",
            "canary",
            100,
            0,
            &GatewayRef {
                name: "platform-gateway",
                namespace: "envoy-gateway-system",
            },
        )
        .unwrap();

        let backends = route["spec"]["rules"][0]["backendRefs"].as_array().unwrap();
        assert_eq!(backends[0]["weight"], 100);
        assert_eq!(backends[1]["weight"], 0);
    }

    #[test]
    fn header_match_httproute_structure() {
        let headers = std::collections::HashMap::from([(
            "x-experiment".to_string(),
            "treatment".to_string(),
        )]);

        let route = build_header_match_httproute(
            "checkout-ab",
            "myapp-prod",
            "checkout.example.com",
            "checkout-control",
            "checkout-treatment",
            &headers,
            &GatewayRef {
                name: "platform-gateway",
                namespace: "envoy-gateway-system",
            },
        );

        assert_eq!(route["kind"], "HTTPRoute");

        // parentRefs should reference the shared gateway cross-namespace
        let parent_refs = route["spec"]["parentRefs"].as_array().unwrap();
        assert_eq!(parent_refs.len(), 1);
        assert_eq!(parent_refs[0]["name"], "platform-gateway");
        assert_eq!(parent_refs[0]["namespace"], "envoy-gateway-system");

        let rules = route["spec"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);

        // First rule: header match → treatment
        let matches = rules[0]["matches"][0]["headers"].as_array().unwrap();
        assert_eq!(matches[0]["name"], "x-experiment");
        assert_eq!(matches[0]["value"], "treatment");
        assert_eq!(rules[0]["backendRefs"][0]["name"], "checkout-treatment");
        assert_eq!(rules[0]["backendRefs"][0]["port"], 8080);

        // Second rule: default → control
        assert!(rules[1]["matches"].is_null());
        assert_eq!(rules[1]["backendRefs"][0]["name"], "checkout-control");
        assert_eq!(rules[1]["backendRefs"][0]["port"], 8080);
    }

    #[test]
    fn weighted_httproute_rejects_bad_weights() {
        let err = build_weighted_httproute(
            "api-route",
            "ns",
            "api.example.com",
            "stable",
            "canary",
            80,
            30,
            &GatewayRef {
                name: "platform-gateway",
                namespace: "envoy-gateway-system",
            },
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("must sum to 100"),
            "expected weight-sum error, got: {msg}"
        );
    }

    #[test]
    fn weighted_httproute_100_percent_canary() {
        let route = build_weighted_httproute(
            "api-route",
            "ns",
            "api.example.com",
            "stable",
            "canary",
            0,
            100,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        )
        .unwrap();

        let backends = route["spec"]["rules"][0]["backendRefs"].as_array().unwrap();
        assert_eq!(backends[0]["weight"], 0);
        assert_eq!(backends[1]["weight"], 100);
    }

    #[test]
    fn weighted_httproute_wildcard_hostname_no_hostnames() {
        let route = build_weighted_httproute(
            "api-route",
            "ns",
            "*",
            "stable",
            "canary",
            50,
            50,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        )
        .unwrap();

        // Wildcard "*" hostname should not add hostnames to spec
        assert!(route["spec"]["hostnames"].is_null());
    }

    #[test]
    fn weighted_httproute_real_hostname_has_hostnames() {
        let route = build_weighted_httproute(
            "api-route",
            "ns",
            "api.example.com",
            "stable",
            "canary",
            50,
            50,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        )
        .unwrap();

        let hostnames = route["spec"]["hostnames"].as_array().unwrap();
        assert_eq!(hostnames.len(), 1);
        assert_eq!(hostnames[0], "api.example.com");
    }

    #[test]
    fn weighted_httproute_rejects_zero_sum() {
        let err = build_weighted_httproute(
            "api-route",
            "ns",
            "api.example.com",
            "stable",
            "canary",
            0,
            0,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("must sum to 100"));
    }

    #[test]
    fn weighted_httproute_has_managed_by_label() {
        let route = build_weighted_httproute(
            "api-route",
            "ns",
            "*",
            "stable",
            "canary",
            50,
            50,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        )
        .unwrap();

        assert_eq!(
            route["metadata"]["labels"]["platform.io/managed-by"],
            "platform-deployer"
        );
    }

    #[test]
    fn header_match_httproute_wildcard_no_hostnames() {
        let headers = std::collections::HashMap::from([("x-flag".to_string(), "on".to_string())]);
        let route = build_header_match_httproute(
            "ab-route",
            "ns",
            "*",
            "control",
            "treatment",
            &headers,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        );
        assert!(route["spec"]["hostnames"].is_null());
    }

    #[test]
    fn header_match_httproute_real_hostname_has_hostnames() {
        let headers = std::collections::HashMap::from([("x-flag".to_string(), "on".to_string())]);
        let route = build_header_match_httproute(
            "ab-route",
            "ns",
            "test.example.com",
            "control",
            "treatment",
            &headers,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        );
        let hostnames = route["spec"]["hostnames"].as_array().unwrap();
        assert_eq!(hostnames[0], "test.example.com");
    }

    #[test]
    fn header_match_httproute_multiple_headers() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("x-experiment".to_string(), "treatment".to_string());
        headers.insert("x-variant".to_string(), "B".to_string());

        let route = build_header_match_httproute(
            "ab-route",
            "ns",
            "*",
            "control",
            "treatment",
            &headers,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        );

        let rules = route["spec"]["rules"].as_array().unwrap();
        let header_matches = rules[0]["matches"][0]["headers"].as_array().unwrap();
        assert_eq!(header_matches.len(), 2);
    }

    #[test]
    fn header_match_httproute_empty_headers() {
        let headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        let route = build_header_match_httproute(
            "ab-route",
            "ns",
            "*",
            "control",
            "treatment",
            &headers,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        );

        let rules = route["spec"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        let header_matches = rules[0]["matches"][0]["headers"].as_array().unwrap();
        assert!(header_matches.is_empty());
    }

    #[test]
    fn header_match_httproute_has_managed_by_label() {
        let headers = std::collections::HashMap::new();
        let route = build_header_match_httproute(
            "ab-route",
            "ns",
            "*",
            "control",
            "treatment",
            &headers,
            &GatewayRef {
                name: "gw",
                namespace: "gw-system",
            },
        );
        assert_eq!(
            route["metadata"]["labels"]["platform.io/managed-by"],
            "platform-deployer"
        );
    }
}
