// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `platform_k8s::namespace` — pure logic, no K8s cluster needed.

use platform_k8s::build_network_policy;

// ---------------------------------------------------------------------------
// Network policy allows all TCP between mesh namespaces (not just 8443)
// ---------------------------------------------------------------------------

#[test]
fn network_policy_mesh_allows_all_tcp() {
    let np = build_network_policy("my-app-dev", "platform");

    // Ingress: mesh rule should allow all TCP (no specific port)
    let ingress = np["spec"]["ingress"]
        .as_array()
        .expect("should have ingress");
    let mesh_ingress = &ingress[0];
    let ingress_from =
        &mesh_ingress["from"][0]["namespaceSelector"]["matchLabels"]["platform.io/managed-by"];
    assert_eq!(ingress_from, "platform");
    let ingress_ports = mesh_ingress["ports"].as_array().expect("should have ports");
    assert_eq!(ingress_ports.len(), 1);
    assert_eq!(ingress_ports[0]["protocol"], "TCP");
    // Should NOT have a specific port number — allows all TCP
    assert!(
        ingress_ports[0].get("port").is_none(),
        "mesh ingress should allow all TCP, not just a specific port"
    );

    // Egress: mesh rule should allow all TCP (not just 8443)
    let egress = np["spec"]["egress"].as_array().expect("should have egress");
    // Find the mesh egress rule (to platform-managed namespaces)
    let mesh_egress = egress
        .iter()
        .find(|rule| {
            rule["to"]
                .as_array()
                .and_then(|to| to.first())
                .and_then(|t| {
                    t["namespaceSelector"]["matchLabels"]["platform.io/managed-by"].as_str()
                })
                .is_some()
        })
        .expect("should have mesh egress rule");
    let egress_ports = mesh_egress["ports"].as_array().expect("should have ports");
    assert_eq!(egress_ports[0]["protocol"], "TCP");
    assert!(
        egress_ports[0].get("port").is_none(),
        "mesh egress should allow all TCP, not just a specific port"
    );
}
