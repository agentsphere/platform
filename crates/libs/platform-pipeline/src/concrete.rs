// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Concrete [`PipelineServices`] implementation that delegates to individual
//! trait implementations.
//!
//! Composes [`WebhookDispatcher`], [`OpsRepoManager`], [`ManifestApplier`],
//! and [`RegistryCredentialProvider`] into a single `PipelineServices` impl,
//! avoiding 4+ generic type parameters on every executor function.
//!
//! Auto-merge (formerly `MergeRequestHandler`) is now handled via
//! `PlatformEvent::PipelineCompleted` events dispatched by the eventbus.

use std::path::Path;
use std::sync::Arc;

use uuid::Uuid;

use platform_types::{
    ManifestApplier, OpsRepoManager, RegistryCredentialProvider, WebhookDispatcher,
};

use crate::state::PipelineServices;

/// Concrete [`PipelineServices`] that delegates each method to an underlying
/// trait implementation.
///
/// All inner services are wrapped in `Arc` for cheap cloning (required by the
/// `Clone` bound on `PipelineServices`).
pub struct ConcretePipelineServices<W, O, A, R>
where
    W: WebhookDispatcher,
    O: OpsRepoManager,
    A: ManifestApplier,
    R: RegistryCredentialProvider,
{
    pub webhook_dispatcher: Arc<W>,
    pub ops_repo: Arc<O>,
    pub applier: Arc<A>,
    pub registry: Arc<R>,
}

// Manual Clone impl: all fields are Arc<T>, which is always Clone
// regardless of whether T is Clone.
impl<W, O, A, R> Clone for ConcretePipelineServices<W, O, A, R>
where
    W: WebhookDispatcher,
    O: OpsRepoManager,
    A: ManifestApplier,
    R: RegistryCredentialProvider,
{
    fn clone(&self) -> Self {
        Self {
            webhook_dispatcher: self.webhook_dispatcher.clone(),
            ops_repo: self.ops_repo.clone(),
            applier: self.applier.clone(),
            registry: self.registry.clone(),
        }
    }
}

impl<W, O, A, R> ConcretePipelineServices<W, O, A, R>
where
    W: WebhookDispatcher,
    O: OpsRepoManager,
    A: ManifestApplier,
    R: RegistryCredentialProvider,
{
    pub fn new(
        webhook_dispatcher: Arc<W>,
        ops_repo: Arc<O>,
        applier: Arc<A>,
        registry: Arc<R>,
    ) -> Self {
        Self {
            webhook_dispatcher,
            ops_repo,
            applier,
            registry,
        }
    }
}

impl<W, O, A, R> PipelineServices for ConcretePipelineServices<W, O, A, R>
where
    W: WebhookDispatcher + 'static,
    O: OpsRepoManager + 'static,
    A: ManifestApplier + 'static,
    R: RegistryCredentialProvider + 'static,
{
    async fn fire_webhooks(&self, project_id: Uuid, event_name: &str, payload: &serde_json::Value) {
        self.webhook_dispatcher
            .fire_webhooks(project_id, event_name, payload)
            .await;
    }

    async fn ops_read_file(&self, repo_path: &Path, git_ref: &str, file: &str) -> Option<String> {
        self.ops_repo.read_file(repo_path, git_ref, file).await
    }

    async fn ops_sync_from_project(
        &self,
        project_id: Uuid,
        source: &Path,
        branch: &str,
    ) -> anyhow::Result<()> {
        self.ops_repo
            .sync_from_project(project_id, source, branch)
            .await
    }

    async fn ops_write_file(
        &self,
        repo_path: &Path,
        branch: &str,
        file: &str,
        content: &[u8],
        msg: &str,
    ) -> anyhow::Result<String> {
        self.ops_repo
            .write_file(repo_path, branch, file, content, msg)
            .await
    }

    async fn ops_read_dir_yaml(
        &self,
        repo_path: &Path,
        git_ref: &str,
        dir: &str,
    ) -> Option<String> {
        self.ops_repo.read_dir_yaml(repo_path, git_ref, dir).await
    }

    async fn ops_commit_values(
        &self,
        ops_path: &Path,
        branch: &str,
        values: &[(&str, &str)],
        msg: &str,
    ) -> anyhow::Result<String> {
        self.ops_repo
            .commit_values(ops_path, branch, values, msg)
            .await
    }

    async fn render_and_apply(
        &self,
        kube: &kube::Client,
        manifest: &str,
        vars: &serde_json::Value,
        namespace: &str,
        tracking: Option<&str>,
    ) -> anyhow::Result<()> {
        self.applier
            .render_and_apply(kube, manifest, vars, namespace, tracking)
            .await
    }

    async fn ensure_pull_secret(
        &self,
        kube: &kube::Client,
        ns: &str,
        project_id: Uuid,
    ) -> anyhow::Result<()> {
        self.registry.ensure_pull_secret(kube, ns, project_id).await
    }

    async fn ensure_scoped_tokens(&self, project_id: Uuid, scope: &str) -> anyhow::Result<()> {
        self.registry.ensure_scoped_tokens(project_id, scope).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MockPipelineServices;

    #[tokio::test]
    async fn mock_pipeline_services_is_valid() {
        // Verify MockPipelineServices satisfies PipelineServices bounds.
        let mock = MockPipelineServices::default();
        mock.fire_webhooks(Uuid::nil(), "push", &serde_json::json!({}))
            .await;
        assert!(mock.webhook_calls.lock().unwrap().len() == 1);
    }
}
