//! Gateway API (`HTTPRoute`) builders for traffic splitting.
//!
//! Platform creates/manages `HTTPRoute` resources referencing user-defined services.
//! Uses kube-rs `DynamicObject` — no new crate dependency needed.
#![allow(dead_code)]

use serde_json::json;

/// Build an `HTTPRoute` JSON for weighted traffic splitting between stable and canary services.
///
/// The route sends `stable_weight`% to `stable_service` and `canary_weight`% to `canary_service`.
pub fn build_weighted_httproute(
    name: &str,
    namespace: &str,
    hostname: &str,
    stable_service: &str,
    canary_service: &str,
    stable_weight: u32,
    canary_weight: u32,
) -> serde_json::Value {
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
        "spec": {
            "parentRefs": [{
                "name": format!("{namespace}-gateway"),
                "namespace": namespace,
            }],
            "hostnames": [hostname],
            "rules": [{
                "backendRefs": [
                    {
                        "name": stable_service,
                        "port": 80,
                        "weight": stable_weight,
                    },
                    {
                        "name": canary_service,
                        "port": 80,
                        "weight": canary_weight,
                    },
                ]
            }]
        }
    })
}

/// Build an `HTTPRoute` JSON for header-based routing (A/B testing).
///
/// Requests matching the specified header are routed to the treatment service;
/// all other requests go to the control service.
pub fn build_header_match_httproute<S: std::hash::BuildHasher>(
    name: &str,
    namespace: &str,
    hostname: &str,
    control_service: &str,
    treatment_service: &str,
    headers: &std::collections::HashMap<String, String, S>,
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
        "spec": {
            "parentRefs": [{
                "name": format!("{namespace}-gateway"),
                "namespace": namespace,
            }],
            "hostnames": [hostname],
            "rules": [
                {
                    "matches": [{
                        "headers": header_matches,
                    }],
                    "backendRefs": [{
                        "name": treatment_service,
                        "port": 80,
                    }]
                },
                {
                    "backendRefs": [{
                        "name": control_service,
                        "port": 80,
                    }]
                }
            ]
        }
    })
}

/// Build a Gateway resource for a project namespace.
pub fn build_gateway(namespace: &str) -> serde_json::Value {
    json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "Gateway",
        "metadata": {
            "name": format!("{namespace}-gateway"),
            "namespace": namespace,
            "labels": {
                "platform.io/managed-by": "platform-deployer"
            }
        },
        "spec": {
            "gatewayClassName": "envoy",
            "listeners": [{
                "name": "http",
                "protocol": "HTTP",
                "port": 80,
            }]
        }
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
        );

        assert_eq!(route["apiVersion"], "gateway.networking.k8s.io/v1");
        assert_eq!(route["kind"], "HTTPRoute");
        assert_eq!(route["metadata"]["name"], "api-canary");
        assert_eq!(route["metadata"]["namespace"], "myapp-prod");

        let rules = route["spec"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        let backends = rules[0]["backendRefs"].as_array().unwrap();
        assert_eq!(backends.len(), 2);
        assert_eq!(backends[0]["name"], "api-stable");
        assert_eq!(backends[0]["weight"], 80);
        assert_eq!(backends[1]["name"], "api-canary");
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
        );

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
        );

        assert_eq!(route["kind"], "HTTPRoute");
        let rules = route["spec"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);

        // First rule: header match → treatment
        let matches = rules[0]["matches"][0]["headers"].as_array().unwrap();
        assert_eq!(matches[0]["name"], "x-experiment");
        assert_eq!(matches[0]["value"], "treatment");
        assert_eq!(rules[0]["backendRefs"][0]["name"], "checkout-treatment");

        // Second rule: default → control
        assert!(rules[1]["matches"].is_null());
        assert_eq!(rules[1]["backendRefs"][0]["name"], "checkout-control");
    }

    #[test]
    fn gateway_structure() {
        let gw = build_gateway("myapp-prod");
        assert_eq!(gw["kind"], "Gateway");
        assert_eq!(gw["metadata"]["name"], "myapp-prod-gateway");
        assert_eq!(gw["spec"]["gatewayClassName"], "envoy");
        let listeners = gw["spec"]["listeners"].as_array().unwrap();
        assert_eq!(listeners.len(), 1);
        assert_eq!(listeners[0]["port"], 80);
    }
}
