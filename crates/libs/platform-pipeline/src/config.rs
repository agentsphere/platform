// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Pipeline-specific configuration subset.

/// Configuration subset needed by the pipeline executor.
///
/// Extracted from `src/config.rs::Config` to decouple from the main binary.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Kaniko image for container builds (e.g. `gcr.io/kaniko-project/executor:latest`).
    pub kaniko_image: String,
    /// Git clone init container image.
    pub git_clone_image: String,
    /// URL for agent/pipeline pods to reach the platform API.
    pub platform_api_url: String,
    /// K8s namespace where the platform itself runs.
    pub platform_namespace: String,
    /// Optional namespace prefix for pipeline namespaces.
    pub ns_prefix: Option<String>,
    /// K8s namespace for the gateway/ingress controller.
    pub gateway_namespace: String,
    /// Platform's built-in OCI registry URL (optional).
    pub registry_url: Option<String>,
    /// Node-accessible registry URL (for K8s nodes pulling images).
    pub node_registry_url: Option<String>,
    /// Pipeline-level timeout in seconds.
    pub pipeline_timeout_secs: u64,
    /// Max parallel steps in DAG execution.
    pub pipeline_max_parallel: usize,
    /// Dev mode (relaxes network policies, enables proxy binary).
    pub dev_mode: bool,
    /// Master key for secret decryption.
    pub master_key: Option<String>,
    /// Path to ops repo storage.
    pub ops_repos_path: String,
    /// Optional path to the proxy binary (injected as sidecar in dev mode).
    pub proxy_binary_path: Option<String>,
    /// Legacy fallback namespace for pipelines.
    pub pipeline_namespace: String,
    /// Max size per artifact file in bytes.
    pub max_artifact_file_bytes: u64,
    /// Max total artifact size per pipeline in bytes.
    pub max_artifact_total_bytes: u64,
}

impl PipelineConfig {
    /// Returns the node-accessible registry URL, falling back to `registry_url`.
    pub fn node_registry_url(&self) -> Option<&str> {
        self.node_registry_url
            .as_deref()
            .or(self.registry_url.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PipelineConfig {
        PipelineConfig {
            kaniko_image: "gcr.io/kaniko-project/executor:latest".into(),
            git_clone_image: "alpine/git:latest".into(),
            platform_api_url: "http://platform:8080".into(),
            platform_namespace: "platform".into(),
            ns_prefix: None,
            gateway_namespace: "gateway".into(),
            registry_url: Some("registry.local:5000".into()),
            node_registry_url: None,
            pipeline_timeout_secs: 900,
            pipeline_max_parallel: 4,
            dev_mode: false,
            master_key: None,
            ops_repos_path: "/data/ops-repos".into(),
            proxy_binary_path: None,
            pipeline_namespace: "platform-pipelines".into(),
            max_artifact_file_bytes: 50_000_000,
            max_artifact_total_bytes: 200_000_000,
        }
    }

    #[test]
    fn node_registry_url_falls_back() {
        let config = test_config();
        assert_eq!(config.node_registry_url(), Some("registry.local:5000"));
    }

    #[test]
    fn node_registry_url_prefers_explicit() {
        let mut config = test_config();
        config.node_registry_url = Some("node-registry:5000".into());
        assert_eq!(config.node_registry_url(), Some("node-registry:5000"));
    }

    #[test]
    fn node_registry_url_none() {
        let mut config = test_config();
        config.registry_url = None;
        assert_eq!(config.node_registry_url(), None);
    }
}
