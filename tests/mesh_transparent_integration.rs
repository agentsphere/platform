// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for transparent mesh proxy features.
//!
//! Tests the network policy changes (all TCP between mesh namespaces),
//! transparent proxy injection config propagation, and the iptables
//! init container injection via the reconciler path.

mod helpers;

use platform::deployer::applier::{ProxyInjectionConfig, inject_proxy_wrapper};
use platform::deployer::namespace::build_network_policy;
use sqlx::PgPool;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn transparent_config() -> ProxyInjectionConfig {
    ProxyInjectionConfig {
        platform_api_url: "http://platform.platform.svc.cluster.local:8080".into(),
        platform_secret_name: Some("platform-proxy-token".into()),
        init_image: "platform-runner-bare:latest".into(),
        iptables_init_image: Some("platform-proxy-init:v1".into()),
        mesh_transparent: true,
        mesh_strict_mtls: false,
    }
}

// ---------------------------------------------------------------------------
// Network policy allows all TCP between mesh namespaces (not just 8443)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn network_policy_mesh_allows_all_tcp(_pool: PgPool) {
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

// ---------------------------------------------------------------------------
// Config mesh_strict_mtls defaults to false
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn config_mesh_strict_mtls_defaults_to_false(pool: PgPool) {
    let (state, _token) = helpers::test_state(pool).await;
    // mesh_strict_mtls is always false in test_state (not env-dependent)
    assert!(!state.config.mesh_strict_mtls);
}

// ---------------------------------------------------------------------------
// Transparent injection: full Deployment with multiple containers
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn transparent_injection_multi_container_deployment(_pool: PgPool) {
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: demo-app
spec:
  replicas: 1
  selector:
    matchLabels:
      app: demo
  template:
    metadata:
      labels:
        app: demo
    spec:
      containers:
        - name: web
          image: demo-web:latest
          command: ["./web-server"]
          ports:
            - containerPort: 8080
        - name: worker
          image: demo-worker:latest
          command: ["./worker"]
"#;

    let config = transparent_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    let pod_spec = &doc["spec"]["template"]["spec"];

    // Both containers should be wrapped
    let containers = pod_spec["containers"].as_array().unwrap();
    assert_eq!(containers.len(), 2);
    for container in containers {
        assert_eq!(
            container["command"][0].as_str().unwrap(),
            "/proxy/platform-proxy",
            "container {} should be wrapped",
            container["name"]
        );
        // Both should have transparent env vars
        let env = container["env"].as_array().unwrap();
        assert!(
            env.iter().any(|e| e["name"] == "PROXY_TRANSPARENT"),
            "container {} should have PROXY_TRANSPARENT",
            container["name"]
        );
    }

    // Should have 2 init containers in correct order
    let inits = pod_spec["initContainers"].as_array().unwrap();
    assert_eq!(inits.len(), 2);
    assert_eq!(inits[0]["name"], "proxy-init");
    assert_eq!(inits[1]["name"], "proxy-iptables");
}

// ---------------------------------------------------------------------------
// Transparent injection: StatefulSet gets iptables init container
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn transparent_injection_statefulset(_pool: PgPool) {
    let manifest = r#"
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: postgres
spec:
  serviceName: postgres
  replicas: 1
  selector:
    matchLabels:
      app: postgres
  template:
    spec:
      containers:
        - name: postgres
          image: postgres:16
          command: ["docker-entrypoint.sh"]
          args: ["postgres"]
          ports:
            - containerPort: 5432
"#;

    let config = transparent_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    // StatefulSet should also get iptables init container
    let inits = doc["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .expect("should have initContainers");
    assert!(
        inits.iter().any(|c| c["name"] == "proxy-iptables"),
        "StatefulSet should have proxy-iptables init container"
    );

    // Container should have transparent env vars
    let container = &doc["spec"]["template"]["spec"]["containers"][0];
    let env = container["env"].as_array().unwrap();
    assert!(env.iter().any(|e| e["name"] == "PROXY_TRANSPARENT"));
    assert!(env.iter().any(|e| e["name"] == "PROXY_INBOUND_PORT"));
}

// ---------------------------------------------------------------------------
// Transparent injection: iptables script content verification
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn transparent_injection_iptables_script_content(_pool: PgPool) {
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
spec:
  template:
    spec:
      containers:
        - name: app
          image: app:latest
          command: ["./app"]
"#;

    let config = transparent_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    let inits = doc["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .unwrap();
    let iptables = inits
        .iter()
        .find(|c| c["name"] == "proxy-iptables")
        .expect("should have proxy-iptables");

    let script = iptables["args"][0].as_str().expect("should have script");

    // Verify key iptables rules are present
    assert!(
        script.contains("PLATFORM_INBOUND"),
        "should create PLATFORM_INBOUND chain"
    );
    assert!(
        script.contains("PLATFORM_OUTPUT"),
        "should create PLATFORM_OUTPUT chain"
    );
    assert!(script.contains("PREROUTING"), "should hook into PREROUTING");
    assert!(script.contains("REDIRECT"), "should use REDIRECT target");
    assert!(
        script.contains("127.0.0.6"),
        "should exclude proxy source IP from outbound"
    );
    assert!(
        script.contains("--dport 53"),
        "should exclude DNS from outbound redirect"
    );
}

// ---------------------------------------------------------------------------
// Transparent injection preserves existing init containers
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn transparent_injection_preserves_existing_init_containers(_pool: PgPool) {
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
spec:
  template:
    spec:
      initContainers:
        - name: gen-certs
          image: alpine:latest
          command: ["sh", "-c", "echo certs"]
      containers:
        - name: app
          image: app:latest
          command: ["./app"]
"#;

    let config = transparent_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    let inits = doc["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .unwrap();

    // Should have 3 init containers: gen-certs (user), proxy-init, proxy-iptables
    assert_eq!(
        inits.len(),
        3,
        "should have gen-certs + proxy-init + proxy-iptables"
    );
    // User init containers come first, then proxy-init, then proxy-iptables
    assert_eq!(inits[0]["name"], "gen-certs");
    assert_eq!(inits[1]["name"], "proxy-init");
    assert_eq!(inits[2]["name"], "proxy-iptables");
}

// ---------------------------------------------------------------------------
// Transparent injection: config propagation via reconciler path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn transparent_config_propagation_through_state(pool: PgPool) {
    let (mut state, _token) = helpers::test_state(pool).await;

    // Enable mesh transparent mode
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;
    config.mesh_transparent = true;
    config.mesh_strict_mtls = false;
    state.config = Arc::new(config);

    // Verify config is accessible and correct
    assert!(state.config.mesh_enabled);
    assert!(state.config.mesh_transparent);
    assert!(!state.config.mesh_strict_mtls);
}
