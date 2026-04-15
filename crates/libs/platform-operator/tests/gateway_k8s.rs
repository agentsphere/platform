// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! K8s integration tests for the gateway auto-deploy controller.
//!
//! Each test creates a unique K8s namespace, exercises `reconcile_once()` to
//! create/update the gateway Deployment + Service, and cleans up afterward.

mod helpers;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Namespace, Service};
use kube::api::{Api, DeleteParams, ObjectMeta, PostParams};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use platform_operator::gateway::{ReconcileAction, reconcile_once};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a unique test namespace and return its name.
/// Caller is responsible for cleanup via `cleanup_namespace`.
async fn create_test_namespace(kube: &kube::Client) -> String {
    let name = format!("platform-test-{}", &Uuid::new_v4().to_string()[..8]);
    let ns_api: Api<Namespace> = Api::all(kube.clone());
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            labels: Some(
                [("platform.io/managed-by".to_string(), "platform".to_string())]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    ns_api
        .create(&PostParams::default(), &ns)
        .await
        .unwrap_or_else(|e| panic!("failed to create namespace {name}: {e}"));
    name
}

/// Delete the test namespace (best-effort).
async fn cleanup_namespace(kube: &kube::Client, name: &str) {
    let ns_api: Api<Namespace> = Api::all(kube.clone());
    let _ = ns_api.delete(name, &DeleteParams::default()).await;
}

// ---------------------------------------------------------------------------
// gateway_controller_creates_deployment
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn gateway_controller_creates_deployment(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;
    let ns = create_test_namespace(&state.kube).await;

    let mut config = (*state.config).clone();
    config.gateway_auto_deploy = true;
    config.gateway_namespace = ns.clone();
    config.gateway_http_node_port = 0;
    config.gateway_tls_node_port = 0;
    config.registry_node_url = None;
    config.registry_url = Some("test-registry.local:5000".to_string());
    state.config = Arc::new(config);

    // Run reconcile
    let action = reconcile_once(&state)
        .await
        .expect("reconcile_once should succeed");
    assert_eq!(action, ReconcileAction::Created);

    // Verify Deployment exists
    let deploy_api: Api<Deployment> = Api::namespaced(state.kube.clone(), &ns);
    let deploy = deploy_api
        .get("platform-gateway")
        .await
        .expect("deployment should exist after reconcile");

    // Verify the image is correct
    let image = deploy
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|s| s.containers.first())
        .and_then(|c| c.image.as_ref())
        .expect("container image should be set");
    assert_eq!(image, "test-registry.local:5000/platform-proxy:v1");

    cleanup_namespace(&state.kube, &ns).await;
}

// ---------------------------------------------------------------------------
// gateway_controller_creates_service
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn gateway_controller_creates_service(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;
    let ns = create_test_namespace(&state.kube).await;

    let mut config = (*state.config).clone();
    config.gateway_auto_deploy = true;
    config.gateway_namespace = ns.clone();
    config.gateway_http_node_port = 0;
    config.gateway_tls_node_port = 0;
    config.registry_node_url = None;
    config.registry_url = Some("test-registry.local:5000".to_string());
    state.config = Arc::new(config);

    // Run reconcile — creates both Deployment and Service
    reconcile_once(&state)
        .await
        .expect("reconcile_once should succeed");

    // Verify Service exists
    let svc_api: Api<Service> = Api::namespaced(state.kube.clone(), &ns);
    let svc = svc_api
        .get("platform-gateway")
        .await
        .expect("service should exist after reconcile");

    // Verify NodePort type
    let svc_type = svc
        .spec
        .as_ref()
        .and_then(|s| s.type_.as_ref())
        .expect("service type should be set");
    assert_eq!(svc_type, "NodePort");

    // Verify ports
    let ports = svc
        .spec
        .as_ref()
        .and_then(|s| s.ports.as_ref())
        .expect("service ports should be set");
    assert!(
        ports.len() >= 2,
        "service should have at least 2 ports (http + https)"
    );

    cleanup_namespace(&state.kube, &ns).await;
}

// ---------------------------------------------------------------------------
// gateway_controller_noop_when_current
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn gateway_controller_noop_when_current(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;
    let ns = create_test_namespace(&state.kube).await;

    let mut config = (*state.config).clone();
    config.gateway_auto_deploy = true;
    config.gateway_namespace = ns.clone();
    config.gateway_http_node_port = 0;
    config.gateway_tls_node_port = 0;
    config.registry_node_url = None;
    config.registry_url = Some("test-registry.local:5000".to_string());
    state.config = Arc::new(config);

    // First reconcile — creates resources
    let action1 = reconcile_once(&state)
        .await
        .expect("first reconcile should succeed");
    assert_eq!(action1, ReconcileAction::Created);

    // Second reconcile — should be no-op (image unchanged)
    let action2 = reconcile_once(&state)
        .await
        .expect("second reconcile should succeed");
    assert_eq!(action2, ReconcileAction::NoOp);

    cleanup_namespace(&state.kube, &ns).await;
}

// ---------------------------------------------------------------------------
// gateway_controller_updates_image
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn gateway_controller_updates_image(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;
    let ns = create_test_namespace(&state.kube).await;

    // First reconcile with one registry URL
    let mut config = (*state.config).clone();
    config.gateway_auto_deploy = true;
    config.gateway_namespace = ns.clone();
    config.gateway_http_node_port = 0;
    config.gateway_tls_node_port = 0;
    config.registry_node_url = None;
    config.registry_url = Some("old-registry.local:5000".to_string());
    state.config = Arc::new(config);

    let action1 = reconcile_once(&state)
        .await
        .expect("first reconcile should succeed");
    assert_eq!(action1, ReconcileAction::Created);

    // Verify old image
    let deploy_api: Api<Deployment> = Api::namespaced(state.kube.clone(), &ns);
    let deploy = deploy_api.get("platform-gateway").await.unwrap();
    let old_image = deploy
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|s| s.containers.first())
        .and_then(|c| c.image.as_ref())
        .unwrap();
    assert_eq!(old_image, "old-registry.local:5000/platform-proxy:v1");

    // Second reconcile with updated registry URL
    let mut config = (*state.config).clone();
    config.registry_url = Some("new-registry.local:5000".to_string());
    state.config = Arc::new(config);

    let action2 = reconcile_once(&state)
        .await
        .expect("second reconcile should succeed");
    assert_eq!(action2, ReconcileAction::Updated);

    // Verify image was updated
    let deploy = deploy_api.get("platform-gateway").await.unwrap();
    let new_image = deploy
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|s| s.containers.first())
        .and_then(|c| c.image.as_ref())
        .unwrap();
    assert_eq!(new_image, "new-registry.local:5000/platform-proxy:v1");

    cleanup_namespace(&state.kube, &ns).await;
}
