// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for proxy injection into deployment manifests.
//!
//! Tests `inject_proxy_wrapper()` from `platform_deployer::applier` with realistic
//! manifests to verify that only workload resources (Deployment, `StatefulSet`, etc.)
//! get proxy wrapping while non-workload resources (Service, `ConfigMap`) pass through
//! unchanged.

use platform_deployer::applier::{ProxyInjectionConfig, inject_proxy_wrapper};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn proxy_config() -> ProxyInjectionConfig {
    ProxyInjectionConfig {
        platform_api_url: "http://platform.platform.svc.cluster.local:8080".to_string(),
        init_image: "platform-proxy-init:v1".into(),
        mesh_strict_mtls: false,
    }
}

// ---------------------------------------------------------------------------
// proxy_injection_wraps_demo_postgres_deployment
// ---------------------------------------------------------------------------

/// Assert that a container was proxy-wrapped: command, args, volumeMounts, env.
fn assert_proxy_wrapped(doc: &serde_json::Value, orig_cmd: &str, orig_args: &[&str], svc: &str) {
    let containers = doc["spec"]["template"]["spec"]["containers"]
        .as_array()
        .expect("should have containers");
    assert_eq!(containers.len(), 1);
    let container = &containers[0];

    // Command should be wrapped with proxy
    let command = container["command"]
        .as_array()
        .expect("command should be an array");
    assert_eq!(command[0].as_str().unwrap(), "/proxy/platform-proxy");

    // Args: --wrap -- <original_cmd> <original_args...>
    let args = container["args"]
        .as_array()
        .expect("args should be an array");
    assert_eq!(args[0].as_str().unwrap(), "--wrap");
    assert_eq!(args[1].as_str().unwrap(), "--");
    assert_eq!(args[2].as_str().unwrap(), orig_cmd);
    for (i, expected) in orig_args.iter().enumerate() {
        assert_eq!(args[3 + i].as_str().unwrap(), *expected);
    }

    // Proxy volume mount
    let volume_mounts = container["volumeMounts"]
        .as_array()
        .expect("volumeMounts should exist");
    let proxy_mount = volume_mounts
        .iter()
        .find(|m| m["name"].as_str() == Some("platform-proxy"))
        .expect("proxy volume mount should exist");
    assert_eq!(proxy_mount["mountPath"].as_str().unwrap(), "/proxy");
    assert!(proxy_mount["readOnly"].as_bool().unwrap());

    // Proxy volume
    let volumes = doc["spec"]["template"]["spec"]["volumes"]
        .as_array()
        .expect("volumes should exist");
    let proxy_vol = volumes
        .iter()
        .find(|v| v["name"].as_str() == Some("platform-proxy"))
        .expect("proxy volume should exist");
    assert!(
        !proxy_vol["emptyDir"].is_null(),
        "proxy volume should be emptyDir"
    );

    // Env vars
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
    assert_eq!(service_name_env["value"].as_str().unwrap(), svc);
    assert!(env.iter().any(|e| e["name"] == "PROXY_TRANSPARENT"));

    // Single init container
    let inits = doc["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .expect("should have initContainers");
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0]["name"], "proxy-init");
}

#[test]
fn proxy_injection_wraps_demo_postgres_deployment() {
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
    let doc: serde_json::Value =
        serde_yaml::from_str(&result).expect("result should be valid YAML");
    assert_proxy_wrapped(
        &doc,
        "docker-entrypoint.sh",
        &["postgres"],
        "postgres/postgres",
    );
}

// ---------------------------------------------------------------------------
// proxy_injection_preserves_service_unchanged
// ---------------------------------------------------------------------------

#[test]
fn proxy_injection_preserves_service_unchanged() {
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

// ---------------------------------------------------------------------------
// Init container has NET_ADMIN capability
// ---------------------------------------------------------------------------

#[test]
fn proxy_injection_init_container_has_net_admin() {
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web-app
  namespace: demo-dev
spec:
  replicas: 1
  selector:
    matchLabels:
      app: web
  template:
    spec:
      containers:
        - name: web
          image: myapp:latest
          command: ["./server"]
          ports:
            - containerPort: 8080
"#;

    let config = proxy_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value =
        serde_yaml::from_str(&result).expect("result should be valid YAML");

    // Single init container
    let init_containers = doc["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .expect("should have initContainers");
    assert_eq!(init_containers.len(), 1);
    assert_eq!(init_containers[0]["name"], "proxy-init");

    // Has NET_ADMIN capability for iptables
    let caps = init_containers[0]["securityContext"]["capabilities"]["add"]
        .as_array()
        .expect("should have capabilities");
    let cap_names: Vec<&str> = caps.iter().filter_map(|c| c.as_str()).collect();
    assert!(cap_names.contains(&"NET_ADMIN"), "should have NET_ADMIN");
    assert!(cap_names.contains(&"NET_RAW"), "should have NET_RAW");

    // Should NOT allow privilege escalation
    assert_eq!(
        init_containers[0]["securityContext"]["allowPrivilegeEscalation"].as_bool(),
        Some(false)
    );
}

// ---------------------------------------------------------------------------
// Transparent env vars always present
// ---------------------------------------------------------------------------

#[test]
fn proxy_injection_always_adds_transparent_env_vars() {
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web-app
spec:
  template:
    spec:
      containers:
        - name: web
          image: myapp:latest
          command: ["./server"]
"#;

    let config = proxy_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    let container = &doc["spec"]["template"]["spec"]["containers"][0];
    let env = container["env"].as_array().expect("env should exist");

    let env_map: std::collections::HashMap<&str, &str> = env
        .iter()
        .filter_map(|e| Some((e["name"].as_str()?, e["value"].as_str()?)))
        .collect();

    assert_eq!(env_map.get("PROXY_TRANSPARENT"), Some(&"true"));
    assert_eq!(env_map.get("PROXY_MTLS_MODE"), Some(&"permissive"));
    assert_eq!(env_map.get("PROXY_INBOUND_PORT"), Some(&"15006"));
    assert!(env_map.contains_key("PROXY_INTERNAL_CIDRS"));
    // PROXY_OUTBOUND_BIND removed — bypass uses source port range now
    assert!(!env_map.contains_key("PROXY_OUTBOUND_BIND"));
}

// ---------------------------------------------------------------------------
// Strict mTLS mode
// ---------------------------------------------------------------------------

#[test]
fn proxy_injection_strict_mtls() {
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web-app
spec:
  template:
    spec:
      containers:
        - name: web
          image: myapp:latest
          command: ["./server"]
"#;

    let mut config = proxy_config();
    config.mesh_strict_mtls = true;
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    let container = &doc["spec"]["template"]["spec"]["containers"][0];
    let env = container["env"].as_array().expect("env should exist");
    let mtls_mode = env
        .iter()
        .find(|e| e["name"] == "PROXY_MTLS_MODE")
        .expect("PROXY_MTLS_MODE should be set");
    assert_eq!(mtls_mode["value"], "strict");
}

// ---------------------------------------------------------------------------
// Injection: full Deployment with multiple containers (from mesh_transparent)
// ---------------------------------------------------------------------------

#[test]
fn injection_multi_container_deployment() {
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

    let config = proxy_config();
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

    // Single combined init container
    let inits = pod_spec["initContainers"].as_array().unwrap();
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0]["name"], "proxy-init");
}

// ---------------------------------------------------------------------------
// Injection: StatefulSet gets init container (from mesh_transparent)
// ---------------------------------------------------------------------------

#[test]
fn injection_statefulset() {
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

    let config = proxy_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    // StatefulSet should have the init container
    let inits = doc["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .expect("should have initContainers");
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0]["name"], "proxy-init");

    // Container should have transparent env vars
    let container = &doc["spec"]["template"]["spec"]["containers"][0];
    let env = container["env"].as_array().unwrap();
    assert!(env.iter().any(|e| e["name"] == "PROXY_TRANSPARENT"));
    assert!(env.iter().any(|e| e["name"] == "PROXY_INBOUND_PORT"));
}

// ---------------------------------------------------------------------------
// Injection preserves existing init containers (from mesh_transparent)
// ---------------------------------------------------------------------------

#[test]
fn injection_preserves_existing_init_containers() {
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

    let config = proxy_config();
    let result = inject_proxy_wrapper(manifest, &config).expect("injection should succeed");
    let doc: serde_json::Value = serde_yaml::from_str(&result).unwrap();

    let inits = doc["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .unwrap();

    // Should have 2 init containers: gen-certs (user) + proxy-init
    assert_eq!(inits.len(), 2, "should have gen-certs + proxy-init");
    assert_eq!(inits[0]["name"], "gen-certs");
    assert_eq!(inits[1]["name"], "proxy-init");
}
