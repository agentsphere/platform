// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for proxy injection into deployment manifests.
//!
//! Tests `inject_proxy_wrapper()` from `src/deployer/applier.rs` with realistic
//! manifests to verify that only workload resources (Deployment, StatefulSet, etc.)
//! get proxy wrapping while non-workload resources (Service, ConfigMap) pass through
//! unchanged.

mod helpers;

use platform::deployer::applier::{ProxyInjectionConfig, inject_proxy_wrapper};
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn proxy_config() -> ProxyInjectionConfig {
    ProxyInjectionConfig {
        platform_api_url: "http://platform.platform.svc.cluster.local:8080".to_string(),
        platform_secret_name: Some("test-platform-secret".into()),
        init_image: "busybox:stable".into(),
    }
}

// ---------------------------------------------------------------------------
// proxy_injection_wraps_demo_postgres_deployment
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_injection_wraps_demo_postgres_deployment(_pool: PgPool) {
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: postgres
  namespace: demo-dev
spec:
  replicas: 1
  selector:
    matchLabels:
      app: postgres
  template:
    metadata:
      labels:
        app: postgres
    spec:
      containers:
        - name: postgres
          image: postgres:16
          command: ["docker-entrypoint.sh"]
          args: ["postgres"]
          ports:
            - containerPort: 5432
          env:
            - name: POSTGRES_DB
              value: demo
            - name: POSTGRES_USER
              value: demo
"#;

    let config = proxy_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");

    // Parse the result to verify injection
    let doc: serde_json::Value =
        serde_yaml::from_str(&result).expect("result should be valid YAML");

    let containers = doc["spec"]["template"]["spec"]["containers"]
        .as_array()
        .expect("should have containers");
    assert_eq!(containers.len(), 1);

    let container = &containers[0];

    // Command should be wrapped with proxy
    let command = container["command"]
        .as_array()
        .expect("command should be an array");
    assert_eq!(
        command[0].as_str().unwrap(),
        "/proxy/platform-proxy",
        "command should be the proxy binary"
    );

    // Args should include --wrap -- followed by original command + args
    let args = container["args"]
        .as_array()
        .expect("args should be an array");
    assert_eq!(args[0].as_str().unwrap(), "--wrap");
    assert_eq!(args[1].as_str().unwrap(), "--");
    assert_eq!(
        args[2].as_str().unwrap(),
        "docker-entrypoint.sh",
        "original command should follow --"
    );
    assert_eq!(
        args[3].as_str().unwrap(),
        "postgres",
        "original args should be preserved"
    );

    // Verify proxy volume mount was added
    let volume_mounts = container["volumeMounts"]
        .as_array()
        .expect("volumeMounts should exist");
    let proxy_mount = volume_mounts
        .iter()
        .find(|m| m["name"].as_str() == Some("platform-proxy"))
        .expect("proxy volume mount should exist");
    assert_eq!(proxy_mount["mountPath"].as_str().unwrap(), "/proxy");
    assert_eq!(proxy_mount["readOnly"].as_bool().unwrap(), true);

    // Verify proxy volume was added to pod spec
    let volumes = doc["spec"]["template"]["spec"]["volumes"]
        .as_array()
        .expect("volumes should exist");
    let proxy_vol = volumes
        .iter()
        .find(|v| v["name"].as_str() == Some("platform-proxy"))
        .expect("proxy volume should exist");
    assert_eq!(
        proxy_vol["hostPath"]["path"].as_str().unwrap(),
        "/opt/platform/proxy"
    );

    // Verify env vars were added
    let env = container["env"].as_array().expect("env should be an array");
    let api_url_env = env
        .iter()
        .find(|e| e["name"].as_str() == Some("PLATFORM_API_URL"))
        .expect("PLATFORM_API_URL env should be set");
    assert_eq!(
        api_url_env["value"].as_str().unwrap(),
        "http://platform.platform.svc.cluster.local:8080"
    );
    let service_name_env = env
        .iter()
        .find(|e| e["name"].as_str() == Some("PLATFORM_SERVICE_NAME"))
        .expect("PLATFORM_SERVICE_NAME env should be set");
    assert_eq!(
        service_name_env["value"].as_str().unwrap(),
        "postgres/postgres"
    );
}

// ---------------------------------------------------------------------------
// proxy_injection_preserves_service_unchanged
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_injection_preserves_service_unchanged(_pool: PgPool) {
    let manifest = r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  selector:
    matchLabels:
      app: web
  template:
    spec:
      containers:
        - name: web
          image: nginx:1.25
          command: ["nginx"]
          args: ["-g", "daemon off;"]
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
  ports:
    - port: 80
      targetPort: 80
"#;

    let config = proxy_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");

    // Split the multi-doc output and parse each
    let docs: Vec<serde_json::Value> = result
        .split("---")
        .filter(|s| !s.trim().is_empty())
        .map(|s| serde_yaml::from_str(s).expect("each doc should be valid YAML"))
        .collect();
    assert_eq!(docs.len(), 2, "should have exactly 2 documents");

    // First doc = Deployment — should be wrapped
    let deployment = &docs[0];
    assert_eq!(deployment["kind"].as_str().unwrap(), "Deployment");
    let deploy_container = &deployment["spec"]["template"]["spec"]["containers"][0];
    assert_eq!(
        deploy_container["command"][0].as_str().unwrap(),
        "/proxy/platform-proxy",
        "deployment container should be wrapped"
    );

    // Second doc = Service — should be untouched
    let service = &docs[1];
    assert_eq!(service["kind"].as_str().unwrap(), "Service");

    // Service should not have any proxy-related fields
    assert!(
        service.pointer("/spec/template").is_none(),
        "Service has no template — no injection possible"
    );
    // Verify service spec is preserved
    let svc_ports = service["spec"]["ports"]
        .as_array()
        .expect("service ports should be preserved");
    assert_eq!(svc_ports[0]["port"].as_i64().unwrap(), 80);
    assert_eq!(svc_ports[0]["targetPort"].as_i64().unwrap(), 80);
}
