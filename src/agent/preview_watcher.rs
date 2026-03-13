//! K8s Service informer for iframe preview discovery.
//!
//! Watches Services labelled `platform.io/component=iframe-preview` across all
//! namespaces. When a Service with an `iframe` port appears or disappears, a
//! `ProgressEvent` is published to the session's Valkey channel so the frontend
//! can discover/remove iframe panels in real time.

use k8s_openapi::api::core::v1::Service;
use kube::api::Api;
use kube::runtime::watcher;
use uuid::Uuid;

use super::provider::{ProgressEvent, ProgressKind};
use super::pubsub_bridge;
use crate::store::AppState;

/// Run the preview watcher background task.
///
/// Watches all namespaces for Services with `platform.io/component=iframe-preview`.
/// Restarts the watcher stream on error (with backoff).
#[tracing::instrument(skip_all)]
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    use futures_util::TryStreamExt;

    loop {
        let api: Api<Service> = Api::all(state.kube.clone());
        let wc = watcher::Config::default().labels("platform.io/component=iframe-preview");
        let stream = watcher(api, wc);
        tokio::pin!(stream);

        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                event = stream.try_next() => {
                    match event {
                        Ok(Some(watcher::Event::Apply(svc) | watcher::Event::InitApply(svc))) => {
                            handle_service_event(&state, &svc, ProgressKind::IframeAvailable).await;
                        }
                        Ok(Some(watcher::Event::Delete(svc))) => {
                            handle_service_event(&state, &svc, ProgressKind::IframeRemoved).await;
                        }
                        Ok(Some(watcher::Event::Init | watcher::Event::InitDone)) => {
                            // Initial list bookkeeping — no action needed
                        }
                        Ok(None) => break, // stream ended, recreate
                        Err(e) => {
                            tracing::warn!(error = %e, "preview watcher error, restarting");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            break; // break inner loop to recreate stream
                        }
                    }
                }
            }
        }
    }
}

/// Handle a Service event — publish iframe discovery/removal for each iframe port.
async fn handle_service_event(state: &AppState, svc: &Service, kind: ProgressKind) {
    let Some(session_id) = extract_session_id(svc) else {
        return;
    };
    let svc_name = svc.metadata.name.as_deref().unwrap_or("unknown");

    for (port, port_name) in extract_iframe_ports(svc) {
        let event = build_iframe_event(kind.clone(), svc_name, port, &port_name, session_id);
        if let Err(e) = pubsub_bridge::publish_event(&state.valkey, session_id, &event).await {
            tracing::warn!(error = %e, %session_id, kind = ?kind, "failed to publish iframe event");
        }
    }
}

/// Extract the session UUID from the `platform.io/session` label.
fn extract_session_id(svc: &Service) -> Option<Uuid> {
    svc.metadata
        .labels
        .as_ref()?
        .get("platform.io/session")?
        .parse()
        .ok()
}

/// Extract ports named `"iframe"` from a Service spec.
/// Returns `(port_number, port_name)` tuples.
fn extract_iframe_ports(svc: &Service) -> Vec<(i32, String)> {
    let Some(spec) = &svc.spec else {
        return Vec::new();
    };
    let Some(ports) = &spec.ports else {
        return Vec::new();
    };
    ports
        .iter()
        .filter(|p| p.name.as_deref() == Some("iframe"))
        .map(|p| (p.port, p.name.clone().unwrap_or_default()))
        .collect()
}

/// Build a `ProgressEvent` for iframe discovery/removal.
fn build_iframe_event(
    kind: ProgressKind,
    service_name: &str,
    port: i32,
    port_name: &str,
    session_id: Uuid,
) -> ProgressEvent {
    let message = match kind {
        ProgressKind::IframeAvailable => {
            format!("Preview available: {service_name}:{port}")
        }
        ProgressKind::IframeRemoved => {
            format!("Preview removed: {service_name}:{port}")
        }
        _ => String::new(),
    };
    ProgressEvent {
        kind,
        message,
        metadata: Some(serde_json::json!({
            "service_name": service_name,
            "port": port,
            "port_name": port_name,
            "preview_url": format!("/preview/{session_id}/"),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
    use kube::api::ObjectMeta;
    use std::collections::BTreeMap;

    fn make_service(labels: BTreeMap<String, String>, ports: Vec<ServicePort>) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("preview-abc12345".into()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(ports),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn iframe_port() -> ServicePort {
        ServicePort {
            name: Some("iframe".into()),
            port: 8000,
            protocol: Some("TCP".into()),
            ..Default::default()
        }
    }

    fn http_port() -> ServicePort {
        ServicePort {
            name: Some("http".into()),
            port: 80,
            protocol: Some("TCP".into()),
            ..Default::default()
        }
    }

    // -- extract_session_id --

    #[test]
    fn extract_session_id_from_labels() {
        let id = Uuid::new_v4();
        let mut labels = BTreeMap::new();
        labels.insert("platform.io/session".into(), id.to_string());
        let svc = make_service(labels, vec![iframe_port()]);
        assert_eq!(extract_session_id(&svc), Some(id));
    }

    #[test]
    fn extract_session_id_missing_label() {
        let svc = make_service(BTreeMap::new(), vec![iframe_port()]);
        assert_eq!(extract_session_id(&svc), None);
    }

    #[test]
    fn extract_session_id_invalid_uuid() {
        let mut labels = BTreeMap::new();
        labels.insert("platform.io/session".into(), "not-a-uuid".into());
        let svc = make_service(labels, vec![iframe_port()]);
        assert_eq!(extract_session_id(&svc), None);
    }

    // -- extract_iframe_ports --

    #[test]
    fn extract_iframe_ports_filters_correctly() {
        let svc = make_service(BTreeMap::new(), vec![iframe_port(), http_port()]);
        let ports = extract_iframe_ports(&svc);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0], (8000, "iframe".into()));
    }

    #[test]
    fn extract_iframe_ports_no_iframe() {
        let svc = make_service(BTreeMap::new(), vec![http_port()]);
        let ports = extract_iframe_ports(&svc);
        assert!(ports.is_empty());
    }

    #[test]
    fn extract_iframe_ports_unnamed_port_excluded() {
        let port = ServicePort {
            name: None,
            port: 9090,
            protocol: Some("TCP".into()),
            ..Default::default()
        };
        let svc = make_service(BTreeMap::new(), vec![port]);
        assert!(extract_iframe_ports(&svc).is_empty());
    }

    #[test]
    fn extract_iframe_ports_multiple_iframe_ports() {
        let p1 = ServicePort {
            name: Some("iframe".into()),
            port: 8000,
            ..Default::default()
        };
        let p2 = ServicePort {
            name: Some("iframe".into()),
            port: 8001,
            ..Default::default()
        };
        let svc = make_service(BTreeMap::new(), vec![p1, p2]);
        let ports = extract_iframe_ports(&svc);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].0, 8000);
        assert_eq!(ports[1].0, 8001);
    }

    #[test]
    fn extract_iframe_ports_empty_ports_vec() {
        let svc = Service {
            metadata: ObjectMeta::default(),
            spec: Some(ServiceSpec {
                ports: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(extract_iframe_ports(&svc).is_empty());
    }

    #[test]
    fn extract_iframe_ports_empty_spec() {
        let svc = Service {
            metadata: ObjectMeta::default(),
            spec: None,
            ..Default::default()
        };
        let ports = extract_iframe_ports(&svc);
        assert!(ports.is_empty());
    }

    // -- build_iframe_event --

    #[test]
    fn build_iframe_event_available() {
        let id = Uuid::new_v4();
        let event = build_iframe_event(
            ProgressKind::IframeAvailable,
            "preview-abc12345",
            8000,
            "iframe",
            id,
        );
        assert_eq!(event.kind, ProgressKind::IframeAvailable);
        assert!(event.message.contains("Preview available"));
        assert!(event.message.contains("8000"));
        let meta = event.metadata.unwrap();
        assert_eq!(meta["service_name"], "preview-abc12345");
        assert_eq!(meta["port"], 8000);
        assert_eq!(meta["port_name"], "iframe");
        assert_eq!(meta["preview_url"], format!("/preview/{id}/"));
    }

    #[test]
    fn build_iframe_event_removed() {
        let id = Uuid::new_v4();
        let event = build_iframe_event(
            ProgressKind::IframeRemoved,
            "preview-abc12345",
            8000,
            "iframe",
            id,
        );
        assert_eq!(event.kind, ProgressKind::IframeRemoved);
        assert!(event.message.contains("Preview removed"));
    }
}
