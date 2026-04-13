// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Pipeline state for the executor and trigger modules.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use platform_types::traits::TaskHeartbeat;
use uuid::Uuid;

use crate::config::PipelineConfig;

/// Combined trait for all external service dependencies the pipeline executor needs.
///
/// Bundles merge-request handling, webhook dispatch, ops repo management,
/// manifest rendering/applying, and registry credential provisioning into
/// a single trait. This avoids 6 separate generic type parameters on every
/// function in the executor.
///
/// `src/` provides a concrete implementation that delegates to the real
/// deployer, API, and reconciler modules.
pub trait PipelineServices: Send + Sync + Clone + 'static {
    /// Attempt to auto-merge any open MRs for the given project.
    fn try_auto_merge(&self, project_id: Uuid) -> impl Future<Output = ()> + Send;

    /// Fire webhooks for a project event.
    fn fire_webhooks(
        &self,
        project_id: Uuid,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> impl Future<Output = ()> + Send;

    /// Read a file from a git repo at a given ref.
    fn ops_read_file(
        &self,
        repo_path: &Path,
        git_ref: &str,
        file: &str,
    ) -> impl Future<Output = Option<String>> + Send;

    /// Sync an ops repo from a project source repo.
    fn ops_sync_from_project(
        &self,
        project_id: Uuid,
        source: &Path,
        branch: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Write a file to a git repo and commit.
    fn ops_write_file(
        &self,
        repo_path: &Path,
        branch: &str,
        file: &str,
        content: &[u8],
        msg: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;

    /// Read all YAML files in a directory from a git repo at a given ref.
    fn ops_read_dir_yaml(
        &self,
        repo_path: &Path,
        git_ref: &str,
        dir: &str,
    ) -> impl Future<Output = Option<String>> + Send;

    /// Commit key=value pairs to an ops repo.
    fn ops_commit_values(
        &self,
        ops_path: &Path,
        branch: &str,
        values: &[(&str, &str)],
        msg: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;

    /// Render manifests with variables and apply to a K8s namespace.
    fn render_and_apply(
        &self,
        kube: &kube::Client,
        manifest: &str,
        vars: &serde_json::Value,
        namespace: &str,
        tracking: Option<&str>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Ensure a pull secret exists in a namespace for a project.
    fn ensure_pull_secret(
        &self,
        kube: &kube::Client,
        ns: &str,
        project_id: Uuid,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Ensure scoped tokens exist for a project with a given scope.
    fn ensure_scoped_tokens(
        &self,
        project_id: Uuid,
        scope: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Shared state for the pipeline executor.
///
/// Generic over `Svc: PipelineServices` so `src/` can plug in concrete
/// implementations while tests can use mocks.
#[derive(Clone)]
pub struct PipelineState<Svc: PipelineServices> {
    pub pool: sqlx::PgPool,
    pub kube: kube::Client,
    pub valkey: fred::clients::Pool,
    pub minio: opendal::Operator,
    pub config: PipelineConfig,
    pub pipeline_notify: Arc<tokio::sync::Notify>,
    pub task_heartbeat: Arc<dyn TaskHeartbeat>,
    pub services: Svc,
}

// ---------------------------------------------------------------------------
// Test support: mock implementations
// ---------------------------------------------------------------------------

/// No-op heartbeat for tests.
pub struct NoopHeartbeat;

impl TaskHeartbeat for NoopHeartbeat {
    fn register(&self, _name: &str, _expected_interval_secs: u64) {}
    fn heartbeat(&self, _name: &str) {}
    fn report_error(&self, _name: &str, _message: &str) {}
}

/// Mock `PipelineServices` that records calls for assertion.
///
/// All methods succeed with no-op defaults. Recorded data is behind
/// `Arc<Mutex<_>>` so clones share state (required by the `Clone` bound).
#[derive(Clone, Default)]
pub struct MockPipelineServices {
    pub auto_merge_calls: Arc<std::sync::Mutex<Vec<Uuid>>>,
    pub webhook_calls: Arc<std::sync::Mutex<Vec<(Uuid, String, serde_json::Value)>>>,
}

impl PipelineServices for MockPipelineServices {
    async fn try_auto_merge(&self, project_id: Uuid) {
        self.auto_merge_calls.lock().unwrap().push(project_id);
    }

    async fn fire_webhooks(&self, project_id: Uuid, event_name: &str, payload: &serde_json::Value) {
        self.webhook_calls.lock().unwrap().push((
            project_id,
            event_name.to_string(),
            payload.clone(),
        ));
    }

    async fn ops_read_file(
        &self,
        _repo_path: &Path,
        _git_ref: &str,
        _file: &str,
    ) -> Option<String> {
        None
    }

    async fn ops_sync_from_project(
        &self,
        _project_id: Uuid,
        _source: &Path,
        _branch: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn ops_write_file(
        &self,
        _repo_path: &Path,
        _branch: &str,
        _file: &str,
        _content: &[u8],
        _msg: &str,
    ) -> anyhow::Result<String> {
        Ok("mock-sha".to_string())
    }

    async fn ops_read_dir_yaml(
        &self,
        _repo_path: &Path,
        _git_ref: &str,
        _dir: &str,
    ) -> Option<String> {
        None
    }

    async fn ops_commit_values(
        &self,
        _ops_path: &Path,
        _branch: &str,
        _values: &[(&str, &str)],
        _msg: &str,
    ) -> anyhow::Result<String> {
        Ok("mock-sha".to_string())
    }

    async fn render_and_apply(
        &self,
        _kube: &kube::Client,
        _manifest: &str,
        _vars: &serde_json::Value,
        _namespace: &str,
        _tracking: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn ensure_pull_secret(
        &self,
        _kube: &kube::Client,
        _ns: &str,
        _project_id: Uuid,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn ensure_scoped_tokens(&self, _project_id: Uuid, _scope: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
