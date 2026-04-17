// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Production service trait implementations wired from shared infrastructure.
//!
//! Provides owned implementations of [`SecretsResolver`] and
//! [`NotificationDispatcher`] that delegate to the library crate
//! implementations (`platform-secrets`, `platform-notify`).

use std::path::{Path, PathBuf};

use sqlx::PgPool;
use uuid::Uuid;

use platform_types::{
    NotificationDispatcher, NotifyParams, RegistryCredentialProvider, SecretsResolver,
};

// ---------------------------------------------------------------------------
// AppSecretsResolver
// ---------------------------------------------------------------------------

/// Owned [`SecretsResolver`] backed by Postgres + AES-256-GCM master key.
///
/// Delegates to `platform_secrets::engine` free functions.
/// Unlike [`platform_secrets::engine::PgSecretsResolver`] (which borrows),
/// this struct owns its pool and key, making it storable in long-lived state.
#[derive(Clone)]
pub struct AppSecretsResolver {
    pool: PgPool,
    master_key: [u8; 32],
}

impl AppSecretsResolver {
    pub fn new(pool: PgPool, master_key: [u8; 32]) -> Self {
        Self { pool, master_key }
    }
}

impl SecretsResolver for AppSecretsResolver {
    async fn resolve_secret(
        &self,
        project_id: Uuid,
        name: &str,
        requested_scope: &str,
    ) -> anyhow::Result<String> {
        platform_secrets::resolve_secret(
            &self.pool,
            &self.master_key,
            project_id,
            name,
            requested_scope,
        )
        .await
    }

    async fn resolve_secret_hierarchical(
        &self,
        project_id: Uuid,
        workspace_id: Option<Uuid>,
        environment: Option<&str>,
        name: &str,
        requested_scope: &str,
    ) -> anyhow::Result<String> {
        platform_secrets::resolve_secret_hierarchical(
            &self.pool,
            &self.master_key,
            project_id,
            workspace_id,
            environment,
            name,
            requested_scope,
        )
        .await
    }

    async fn resolve_secrets_for_env(
        &self,
        project_id: Uuid,
        scope: &str,
        template: &str,
    ) -> anyhow::Result<String> {
        platform_secrets::resolve_secrets_for_env(
            &self.pool,
            &self.master_key,
            project_id,
            scope,
            template,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// AppRegistryCredentials
// ---------------------------------------------------------------------------

/// Owned [`RegistryCredentialProvider`] backed by Postgres.
///
/// The library's [`platform_registry::RegistryCredentials`] borrows its pool
/// and URL, so it can't be stored in `Arc`. This owned wrapper delegates to
/// a fresh `RegistryCredentials` on each call.
#[derive(Clone)]
pub struct AppRegistryCredentials {
    pool: PgPool,
    registry_url: Option<String>,
}

impl AppRegistryCredentials {
    pub fn new(pool: PgPool, registry_url: Option<String>) -> Self {
        Self { pool, registry_url }
    }
}

impl RegistryCredentialProvider for AppRegistryCredentials {
    async fn ensure_pull_secret(
        &self,
        kube: &kube::Client,
        ns: &str,
        project_id: Uuid,
    ) -> anyhow::Result<()> {
        let url = self.registry_url.as_deref().unwrap_or("");
        platform_registry::RegistryCredentials::new(&self.pool, url)
            .ensure_pull_secret(kube, ns, project_id)
            .await
    }

    async fn ensure_scoped_tokens(&self, project_id: Uuid, scope: &str) -> anyhow::Result<()> {
        let url = self.registry_url.as_deref().unwrap_or("");
        platform_registry::RegistryCredentials::new(&self.pool, url)
            .ensure_scoped_tokens(project_id, scope)
            .await
    }
}

// ---------------------------------------------------------------------------
// AppReconcilerServices
// ---------------------------------------------------------------------------

/// Owned [`ReconcilerServices`](platform_deployer::ReconcilerServices) backed
/// by shared infrastructure.
///
/// Delegates each method to the corresponding workspace crate.  Only `pool`
/// and `master_key` are stored; other deps are constructed per-call or taken
/// as method parameters.
#[derive(Clone)]
pub struct AppReconcilerServices {
    pool: PgPool,
    master_key: Option<[u8; 32]>,
}

impl AppReconcilerServices {
    pub fn new(pool: PgPool, master_key: Option<[u8; 32]>) -> Self {
        Self { pool, master_key }
    }
}

impl platform_deployer::ReconcilerServices for AppReconcilerServices {
    async fn fire_webhooks(&self, project_id: Uuid, event: &str, payload: &serde_json::Value) {
        use platform_types::WebhookDispatcher;
        platform_webhook::WebhookDispatch::new(self.pool.clone())
            .fire_webhooks(project_id, event, payload)
            .await;
    }

    async fn render_and_apply(
        &self,
        kube: &kube::Client,
        manifest: &str,
        vars: &serde_json::Value,
        ns: &str,
        tracking: Option<&str>,
    ) -> anyhow::Result<()> {
        use platform_types::ManifestApplier;
        platform_deployer::DeployerService
            .render_and_apply(kube, manifest, vars, ns, tracking)
            .await
    }

    async fn ops_read_file(&self, repo_path: &Path, git_ref: &str, file: &str) -> Option<String> {
        platform_ops_repo::read_file_at_ref(repo_path, git_ref, file)
            .await
            .ok()
    }

    async fn ops_read_dir_yaml(
        &self,
        repo_path: &Path,
        git_ref: &str,
        dir: &str,
    ) -> Option<String> {
        platform_ops_repo::read_dir_yaml_at_ref(repo_path, git_ref, dir)
            .await
            .ok()
    }

    async fn ops_commit_values(
        &self,
        ops_path: &Path,
        branch: &str,
        values: &[(&str, &str)],
        msg: &str,
    ) -> anyhow::Result<String> {
        use platform_types::OpsRepoManager;
        platform_ops_repo::OpsRepoService::new(self.pool.clone())
            .commit_values(ops_path, branch, values, msg)
            .await
    }

    async fn decrypt_secret(&self, project_id: Uuid, key: &str) -> anyhow::Result<Option<String>> {
        let Some(master_key) = &self.master_key else {
            return Ok(None);
        };
        match platform_secrets::resolve_secret(&self.pool, master_key, project_id, key, "all").await
        {
            Ok(val) => Ok(Some(val)),
            Err(e) if e.to_string().contains("not found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn ensure_namespace(
        &self,
        kube: &kube::Client,
        ns_name: &str,
        env: &str,
        project_id: &str,
        platform_namespace: &str,
        gateway_namespace: &str,
        dev_mode: bool,
    ) -> anyhow::Result<()> {
        platform_k8s::namespace::ensure_namespace(
            kube,
            ns_name,
            env,
            project_id,
            platform_namespace,
            gateway_namespace,
            dev_mode,
        )
        .await
        .map_err(Into::into)
    }

    async fn delete_namespace(&self, kube: &kube::Client, ns_name: &str) -> anyhow::Result<()> {
        platform_k8s::namespace::delete_namespace(kube, ns_name).await
    }

    async fn sync_ops_repo(
        &self,
        pool: &PgPool,
        ops_repo_id: Uuid,
    ) -> anyhow::Result<(PathBuf, String, String)> {
        platform_ops_repo::sync_repo(pool, ops_repo_id)
            .await
            .map_err(Into::into)
    }

    async fn get_branch_sha(&self, repo_path: &Path, branch: &str) -> anyhow::Result<String> {
        platform_ops_repo::get_branch_sha(repo_path, branch)
            .await
            .map_err(Into::into)
    }

    async fn get_head_sha(&self, repo_path: &Path) -> anyhow::Result<String> {
        platform_ops_repo::get_head_sha(repo_path)
            .await
            .map_err(Into::into)
    }

    async fn read_values(
        &self,
        repo_path: &Path,
        branch: &str,
        environment: &str,
    ) -> anyhow::Result<serde_json::Value> {
        platform_ops_repo::read_values(repo_path, branch, environment)
            .await
            .map_err(Into::into)
    }

    fn generate_api_token(&self) -> (String, String) {
        platform_auth::generate_api_token()
    }

    async fn publish_event(
        &self,
        valkey: &fred::clients::Pool,
        event_json: &str,
    ) -> anyhow::Result<()> {
        use fred::interfaces::PubsubInterface;
        valkey
            .next()
            .publish::<(), _, _>(platform_types::events::EVENTS_CHANNEL, event_json)
            .await?;
        Ok(())
    }

    fn check_condition(&self, condition: &str, threshold: Option<f64>, value: Option<f64>) -> bool {
        platform_observe::alert::check_condition(condition, threshold, value)
    }

    async fn evaluate_metric(
        &self,
        pool: &PgPool,
        name: &str,
        labels: Option<&serde_json::Value>,
        agg: &str,
        window_secs: i32,
    ) -> anyhow::Result<Option<f64>> {
        platform_observe::alert::evaluate_metric(pool, name, labels, agg, window_secs)
            .await
            .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// AppNotificationDispatcher
// ---------------------------------------------------------------------------

/// Production [`NotificationDispatcher`] backed by SMTP + Postgres + Valkey.
///
/// Wraps [`platform_notify::SmtpNotificationDispatcher`] with the concrete
/// [`platform_webhook::WebhookDispatch`] webhook dispatcher.
///
/// Wrapped in an inner `Arc` so the struct is cheaply cloneable (required
/// by `PlatformState: Clone`) without requiring `SmtpNotificationDispatcher`
/// itself to derive `Clone`.
#[derive(Clone)]
pub struct AppNotificationDispatcher {
    inner: std::sync::Arc<
        platform_notify::SmtpNotificationDispatcher<platform_webhook::WebhookDispatch>,
    >,
}

impl AppNotificationDispatcher {
    pub fn new(
        pool: PgPool,
        valkey: fred::clients::Pool,
        smtp_config: Option<platform_notify::SmtpConfig>,
        webhook_dispatcher: platform_webhook::WebhookDispatch,
    ) -> Self {
        Self {
            inner: std::sync::Arc::new(platform_notify::SmtpNotificationDispatcher::new(
                pool,
                valkey,
                smtp_config,
                webhook_dispatcher,
            )),
        }
    }
}

impl NotificationDispatcher for AppNotificationDispatcher {
    async fn notify(&self, params: NotifyParams<'_>) -> anyhow::Result<()> {
        self.inner.notify(params).await
    }
}

// ---------------------------------------------------------------------------
// SmtpConfig conversion
// ---------------------------------------------------------------------------

/// Convert the binary's [`platform_types::config::SmtpConfig`] into the
/// notify crate's [`platform_notify::SmtpConfig`].
///
/// Returns `None` if no SMTP host is configured (email sending disabled).
pub fn to_notify_smtp_config(
    cfg: &platform_types::config::SmtpConfig,
) -> Option<platform_notify::SmtpConfig> {
    let host = cfg.smtp_host.as_ref()?;
    Some(platform_notify::SmtpConfig {
        host: host.clone(),
        port: cfg.smtp_port,
        from: cfg.smtp_from.clone(),
        username: cfg.smtp_username.clone(),
        password: cfg.smtp_password.clone(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_resolver_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<AppSecretsResolver>();
    }

    #[test]
    fn notification_dispatcher_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<AppNotificationDispatcher>();
    }

    #[test]
    fn reconciler_services_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<AppReconcilerServices>();
    }

    #[test]
    fn smtp_config_conversion_none_when_no_host() {
        let cfg = platform_types::config::SmtpConfig {
            smtp_host: None,
            smtp_port: 587,
            smtp_from: "test@example.com".into(),
            smtp_username: None,
            smtp_password: None,
        };
        assert!(to_notify_smtp_config(&cfg).is_none());
    }

    #[test]
    fn smtp_config_conversion_with_host() {
        let cfg = platform_types::config::SmtpConfig {
            smtp_host: Some("smtp.example.com".into()),
            smtp_port: 465,
            smtp_from: "noreply@example.com".into(),
            smtp_username: Some("user".into()),
            smtp_password: Some("pass".into()),
        };
        let notify_cfg = to_notify_smtp_config(&cfg).unwrap();
        assert_eq!(notify_cfg.host, "smtp.example.com");
        assert_eq!(notify_cfg.port, 465);
        assert_eq!(notify_cfg.from, "noreply@example.com");
        assert_eq!(notify_cfg.username.as_deref(), Some("user"));
        assert_eq!(notify_cfg.password.as_deref(), Some("pass"));
    }
}
