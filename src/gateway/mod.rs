//! Gateway auto-deployment controller.
//!
//! Background task that ensures the platform-native ingress gateway is running
//! in the cluster. Creates/updates a `Deployment` + `NodePort` `Service` for the
//! `platform-proxy --gateway` binary.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, HTTPGetAction, Probe, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, Patch, PatchParams, PostParams};
use tokio::sync::watch;
use tracing::Instrument;

use crate::config::Config;
use crate::store::AppState;

const COMPONENT_LABEL: &str = "platform.io/component";
const COMPONENT_VALUE: &str = "gateway";
const MANAGED_BY_LABEL: &str = "platform.io/managed-by";
const DEPLOY_NAME: &str = "platform-gateway";

/// Background task: ensure the platform gateway is running in the cluster.
pub async fn reconcile_gateway(state: AppState, mut shutdown: watch::Receiver<()>) {
    // Wait for registry seeding to complete before attempting deployment
    tokio::time::sleep(Duration::from_secs(10)).await;

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    state.task_registry.register("gateway_reconciler", 10);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let span = tracing::info_span!("task_iteration",
                    task_name = "gateway_reconciler", source = "system");
                async {
                    match reconcile_once(&state).await {
                        Ok(action) => {
                            state.task_registry.heartbeat("gateway_reconciler");
                            if action != ReconcileAction::NoOp {
                                tracing::info!(?action, "gateway reconciled");
                            }
                        }
                        Err(e) => {
                            state.task_registry.report_error("gateway_reconciler", &e.to_string());
                            tracing::warn!(error = %e, "gateway reconciliation failed");
                        }
                    }
                }
                .instrument(span)
                .await;
            }
            _ = shutdown.changed() => {
                tracing::info!("gateway reconciler shutting down");
                break;
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileAction {
    NoOp,
    Created,
    Updated,
}

pub async fn reconcile_once(state: &AppState) -> anyhow::Result<ReconcileAction> {
    let config = &state.config;
    let ns = &config.gateway_namespace;
    let image = resolve_gateway_image(config);

    let deploy_api: Api<Deployment> = Api::namespaced(state.kube.clone(), ns);
    let action = if let Some(existing) = deploy_api.get_opt(DEPLOY_NAME).await? {
        maybe_update_image(&deploy_api, &existing, &image).await?
    } else {
        let deployment = build_deployment(config, &image);
        deploy_api
            .create(&PostParams::default(), &deployment)
            .await?;
        tracing::info!(namespace = %ns, image = %image, "created gateway deployment");
        ReconcileAction::Created
    };

    let svc_api: Api<Service> = Api::namespaced(state.kube.clone(), ns);
    if svc_api.get_opt(DEPLOY_NAME).await?.is_none() {
        let service = build_service(config);
        svc_api.create(&PostParams::default(), &service).await?;
        tracing::info!(namespace = %ns, "created gateway service");
    }

    Ok(action)
}

async fn maybe_update_image(
    api: &Api<Deployment>,
    existing: &Deployment,
    desired_image: &str,
) -> anyhow::Result<ReconcileAction> {
    let current_image = existing
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|s| s.containers.first())
        .and_then(|c| c.image.as_ref())
        .map_or("", String::as_str);

    if current_image == desired_image {
        return Ok(ReconcileAction::NoOp);
    }

    tracing::info!(current = %current_image, desired = %desired_image, "updating gateway image");
    let patch = serde_json::json!({
        "spec": { "template": { "spec": { "containers": [{
            "name": "gateway", "image": desired_image,
        }]}}}
    });
    api.patch(
        DEPLOY_NAME,
        &PatchParams::apply("platform-gateway-controller"),
        &Patch::Strategic(patch),
    )
    .await?;
    Ok(ReconcileAction::Updated)
}

fn resolve_gateway_image(config: &Config) -> String {
    if let Some(ref registry_url) = config.registry_url {
        format!("{registry_url}/platform-proxy:latest")
    } else {
        "platform-proxy:latest".into()
    }
}

fn build_deployment(config: &Config, image: &str) -> Deployment {
    let ns = &config.gateway_namespace;
    let labels = gateway_labels();
    let container = build_gateway_container(config, image);

    Deployment {
        metadata: ObjectMeta {
            name: Some(DEPLOY_NAME.into()),
            namespace: Some(ns.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            template: k8s_openapi::api::core::v1::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(k8s_openapi::api::core::v1::PodSpec {
                    containers: vec![container],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_gateway_container(config: &Config, image: &str) -> Container {
    let watch_ns = config.gateway_watch_namespaces.join(",");

    let env = |name: &str, value: String| EnvVar {
        name: name.into(),
        value: Some(value),
        ..Default::default()
    };

    Container {
        name: "gateway".into(),
        image: Some(image.into()),
        args: Some(vec!["--gateway".into()]),
        env: Some(vec![
            env(
                "PROXY_GATEWAY_HTTP_PORT",
                config.gateway_http_port.to_string(),
            ),
            env(
                "PROXY_GATEWAY_TLS_PORT",
                config.gateway_tls_port.to_string(),
            ),
            env("PROXY_GATEWAY_NAME", config.gateway_name.clone()),
            env("PROXY_GATEWAY_NAMESPACE", config.gateway_namespace.clone()),
            env("PROXY_GATEWAY_WATCH_NAMESPACES", watch_ns),
            env("PLATFORM_API_URL", config.platform_api_url.clone()),
            env("PLATFORM_SERVICE_NAME", "platform-gateway".into()),
        ]),
        ports: Some(vec![
            ContainerPort {
                container_port: i32::from(config.gateway_http_port),
                name: Some("http".into()),
                ..Default::default()
            },
            ContainerPort {
                container_port: i32::from(config.gateway_tls_port),
                name: Some("https".into()),
                ..Default::default()
            },
            ContainerPort {
                container_port: 15020,
                name: Some("health".into()),
                ..Default::default()
            },
        ]),
        readiness_probe: Some(health_probe("/readyz", 5)),
        liveness_probe: Some(health_probe("/healthz", 10)),
        ..Default::default()
    }
}

fn health_probe(path: &str, initial_delay: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::Int(15020),
            ..Default::default()
        }),
        initial_delay_seconds: Some(initial_delay),
        period_seconds: Some(10),
        ..Default::default()
    }
}

fn build_service(config: &Config) -> Service {
    let mk_port = |name: &str, port: u16, node_port: u16| {
        let mut sp = ServicePort {
            name: Some(name.into()),
            port: i32::from(port),
            target_port: Some(IntOrString::Int(i32::from(port))),
            ..Default::default()
        };
        if node_port > 0 {
            sp.node_port = Some(i32::from(node_port));
        }
        sp
    };

    Service {
        metadata: ObjectMeta {
            name: Some(DEPLOY_NAME.into()),
            namespace: Some(config.gateway_namespace.clone()),
            labels: Some(gateway_labels()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("NodePort".into()),
            selector: Some(gateway_labels()),
            ports: Some(vec![
                mk_port(
                    "http",
                    config.gateway_http_port,
                    config.gateway_http_node_port,
                ),
                mk_port(
                    "https",
                    config.gateway_tls_port,
                    config.gateway_tls_node_port,
                ),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn gateway_labels() -> BTreeMap<String, String> {
    BTreeMap::from([
        (COMPONENT_LABEL.into(), COMPONENT_VALUE.into()),
        (MANAGED_BY_LABEL.into(), "platform".into()),
    ])
}
