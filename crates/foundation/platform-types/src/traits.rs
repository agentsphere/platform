// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Cross-module trait contracts.
//!
//! These traits define the communication boundaries between domain crates.
//! Domain crates depend on these traits (via `platform-types`), not on each
//! other. Concrete implementations live in their respective modules or `src/`.
//!
//! Uses `impl Future` return types (stable Rust 1.75+, edition 2024) — no
//! `async-trait` crate needed. Works with generics (`&impl Trait`), not
//! `dyn` dispatch.

use std::future::Future;

use uuid::Uuid;

use crate::audit::AuditEntry;

// Note: `PermissionChecker` is defined in `auth_user.rs` alongside `AuthUser`
// because it's tightly coupled to the auth user context. Re-exported from
// `crate::auth_user::PermissionChecker`.

/// Trait for fire-and-forget audit logging.
///
/// Decouples domain crates from the concrete `AuditLog` implementation
/// (which requires a `PgPool` and `tokio::spawn`).
pub trait AuditLogger: Send + Sync {
    fn send_audit(&self, entry: AuditEntry);
}

/// Trait for resolving decrypted secrets.
///
/// Decouples domain crates (pipeline executor, agent identity, deployer)
/// from the concrete secrets engine implementation.
pub trait SecretsResolver: Send + Sync {
    /// Resolve a secret by name for a project, enforcing scope.
    fn resolve_secret(
        &self,
        project_id: Uuid,
        name: &str,
        requested_scope: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;

    /// Resolve a secret using the full hierarchy (project+env > project > workspace > global).
    fn resolve_secret_hierarchical(
        &self,
        project_id: Uuid,
        workspace_id: Option<Uuid>,
        environment: Option<&str>,
        name: &str,
        requested_scope: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;

    /// Replace `${{ secrets.NAME }}` patterns in a template string.
    fn resolve_secrets_for_env(
        &self,
        project_id: Uuid,
        scope: &str,
        template: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// Parameters for dispatching a notification.
pub struct NotifyParams<'a> {
    pub user_id: Uuid,
    pub notification_type: &'a str,
    pub subject: &'a str,
    pub body: Option<&'a str>,
    pub channel: &'a str,
    pub ref_type: Option<&'a str>,
    pub ref_id: Option<Uuid>,
}

/// Trait for dispatching notifications (email, in-app, webhook).
///
/// Decouples alert evaluation and other event producers from the concrete
/// notification dispatch implementation.
pub trait NotificationDispatcher: Send + Sync {
    /// Send a notification to a user.
    fn notify(&self, params: NotifyParams<'_>) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Trait for checking workspace membership.
///
/// Decouples domain crates (registry access control) from the concrete
/// workspace service implementation.
pub trait WorkspaceMembershipChecker: Send + Sync {
    fn is_member(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> impl Future<Output = anyhow::Result<bool>> + Send;
}

/// Trait for dispatching webhooks to external URLs.
///
/// Decouples domain crates from the concrete webhook dispatch implementation
/// (HTTP client, HMAC signing, concurrency control).
pub trait WebhookDispatcher: Send + Sync {
    /// Fire webhooks for a project event.
    fn fire_webhooks(
        &self,
        project_id: Uuid,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> impl Future<Output = ()> + Send;
}

/// Trait for background task heartbeat tracking.
///
/// Decouples domain crates (agent reaper, pipeline executor) from the
/// concrete `TaskRegistry` implementation in `src/health/`.
pub trait TaskHeartbeat: Send + Sync {
    /// Register a task with its expected heartbeat interval.
    fn register(&self, name: &str, expected_interval_secs: u64);
    /// Record a successful heartbeat for a named task.
    fn heartbeat(&self, name: &str);
    /// Record an error for a named task.
    fn report_error(&self, name: &str, message: &str);
}

/// Trait for auto-merging open MRs after a successful pipeline.
///
/// Decouples the pipeline executor from the merge request API implementation.
pub trait MergeRequestHandler: Send + Sync {
    /// Attempt to auto-merge any open MRs for the given project.
    fn try_auto_merge(&self, project_id: Uuid) -> impl Future<Output = ()> + Send;
}

/// Trait for `GitOps` operations (ops repo read/write/sync).
///
/// Decouples the pipeline executor from the deployer's ops repo implementation.
pub trait OpsRepoManager: Send + Sync {
    /// Read a file from a git repo at a given ref.
    fn read_file(
        &self,
        repo_path: &std::path::Path,
        git_ref: &str,
        file: &str,
    ) -> impl Future<Output = Option<String>> + Send;

    /// Sync an ops repo from a project source repo.
    fn sync_from_project(
        &self,
        project_id: Uuid,
        source: &std::path::Path,
        branch: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Write a file to a git repo and commit.
    fn write_file(
        &self,
        repo_path: &std::path::Path,
        branch: &str,
        file: &str,
        content: &[u8],
        msg: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;

    /// Read all YAML files in a directory from a git repo at a given ref.
    fn read_dir_yaml(
        &self,
        repo_path: &std::path::Path,
        git_ref: &str,
        dir: &str,
    ) -> impl Future<Output = Option<String>> + Send;

    /// Commit key=value pairs to an ops repo.
    fn commit_values(
        &self,
        ops_path: &std::path::Path,
        branch: &str,
        values: &[(&str, &str)],
        msg: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// Trait for rendering manifests and applying them to K8s.
///
/// Decouples the pipeline executor from the deployer's renderer + applier.
#[cfg(feature = "kube")]
pub trait ManifestApplier: Send + Sync {
    /// Render manifests with variables and apply to a K8s namespace.
    fn render_and_apply(
        &self,
        kube: &kube::Client,
        manifest: &str,
        vars: &serde_json::Value,
        namespace: &str,
        tracking: Option<&str>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Trait for providing registry credentials and scoped tokens.
///
/// Decouples the pipeline executor from the deployer's registry/reconciler.
#[cfg(feature = "kube")]
pub trait RegistryCredentialProvider: Send + Sync {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditEntry;

    // Verify traits are object-safe enough for our use case (impl Future, not dyn)
    struct MockAuditLogger;
    impl AuditLogger for MockAuditLogger {
        fn send_audit(&self, _entry: AuditEntry) {}
    }

    struct MockSecretsResolver;
    impl SecretsResolver for MockSecretsResolver {
        async fn resolve_secret(
            &self,
            _project_id: Uuid,
            name: &str,
            _scope: &str,
        ) -> anyhow::Result<String> {
            Ok(format!("mock-{name}"))
        }
        async fn resolve_secret_hierarchical(
            &self,
            _project_id: Uuid,
            _workspace_id: Option<Uuid>,
            _env: Option<&str>,
            name: &str,
            _scope: &str,
        ) -> anyhow::Result<String> {
            Ok(format!("mock-hier-{name}"))
        }
        async fn resolve_secrets_for_env(
            &self,
            _project_id: Uuid,
            _scope: &str,
            template: &str,
        ) -> anyhow::Result<String> {
            Ok(template.to_string())
        }
    }

    struct MockNotificationDispatcher;
    impl NotificationDispatcher for MockNotificationDispatcher {
        async fn notify(&self, _params: NotifyParams<'_>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct MockWebhookDispatcher;
    impl WebhookDispatcher for MockWebhookDispatcher {
        async fn fire_webhooks(
            &self,
            _project_id: Uuid,
            _event_name: &str,
            _payload: &serde_json::Value,
        ) {
        }
    }

    struct MockWorkspaceMembershipChecker;
    impl WorkspaceMembershipChecker for MockWorkspaceMembershipChecker {
        async fn is_member(&self, _workspace_id: Uuid, _user_id: Uuid) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    struct MockTaskHeartbeat;
    impl TaskHeartbeat for MockTaskHeartbeat {
        fn register(&self, _name: &str, _expected_interval_secs: u64) {}
        fn heartbeat(&self, _name: &str) {}
        fn report_error(&self, _name: &str, _message: &str) {}
    }

    struct MockMergeRequestHandler;
    impl MergeRequestHandler for MockMergeRequestHandler {
        async fn try_auto_merge(&self, _project_id: Uuid) {}
    }

    struct MockOpsRepoManager;
    impl OpsRepoManager for MockOpsRepoManager {
        async fn read_file(
            &self,
            _repo_path: &std::path::Path,
            _git_ref: &str,
            _file: &str,
        ) -> Option<String> {
            None
        }
        async fn sync_from_project(
            &self,
            _project_id: Uuid,
            _source: &std::path::Path,
            _branch: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn write_file(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _file: &str,
            _content: &[u8],
            _msg: &str,
        ) -> anyhow::Result<String> {
            Ok("abc123".into())
        }
        async fn read_dir_yaml(
            &self,
            _repo_path: &std::path::Path,
            _git_ref: &str,
            _dir: &str,
        ) -> Option<String> {
            None
        }
        async fn commit_values(
            &self,
            _ops_path: &std::path::Path,
            _branch: &str,
            _values: &[(&str, &str)],
            _msg: &str,
        ) -> anyhow::Result<String> {
            Ok("abc123".into())
        }
    }

    #[cfg(feature = "kube")]
    struct MockManifestApplier;
    #[cfg(feature = "kube")]
    impl ManifestApplier for MockManifestApplier {
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
    }

    #[cfg(feature = "kube")]
    struct MockRegistryCredentialProvider;
    #[cfg(feature = "kube")]
    impl RegistryCredentialProvider for MockRegistryCredentialProvider {
        async fn ensure_pull_secret(
            &self,
            _kube: &kube::Client,
            _ns: &str,
            _project_id: Uuid,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn ensure_scoped_tokens(
            &self,
            _project_id: Uuid,
            _scope: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mock_audit_logger_works() {
        let logger = MockAuditLogger;
        logger.send_audit(AuditEntry {
            actor_id: Uuid::nil(),
            actor_name: "test".into(),
            action: "test.action".into(),
            resource: "test".into(),
            resource_id: None,
            project_id: None,
            detail: None,
            ip_addr: None,
        });
    }

    #[tokio::test]
    async fn mock_secrets_resolver_works() {
        let resolver = MockSecretsResolver;
        let val = resolver
            .resolve_secret(Uuid::nil(), "DB_URL", "pipeline")
            .await
            .unwrap();
        assert_eq!(val, "mock-DB_URL");
    }

    #[tokio::test]
    async fn mock_notification_dispatcher_works() {
        let dispatcher = MockNotificationDispatcher;
        dispatcher
            .notify(NotifyParams {
                user_id: Uuid::nil(),
                notification_type: "test",
                subject: "subject",
                body: None,
                channel: "in_app",
                ref_type: None,
                ref_id: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mock_webhook_dispatcher_works() {
        let dispatcher = MockWebhookDispatcher;
        dispatcher
            .fire_webhooks(Uuid::nil(), "push", &serde_json::json!({}))
            .await;
    }

    #[tokio::test]
    async fn mock_secrets_resolver_hierarchical() {
        let resolver = MockSecretsResolver;
        let val = resolver
            .resolve_secret_hierarchical(
                Uuid::nil(),
                Some(Uuid::nil()),
                Some("production"),
                "API_KEY",
                "pipeline",
            )
            .await
            .unwrap();
        assert_eq!(val, "mock-hier-API_KEY");
    }

    #[tokio::test]
    async fn mock_secrets_resolver_for_env() {
        let resolver = MockSecretsResolver;
        let val = resolver
            .resolve_secrets_for_env(Uuid::nil(), "pipeline", "host=${{ secrets.HOST }}")
            .await
            .unwrap();
        assert_eq!(val, "host=${{ secrets.HOST }}");
    }

    #[tokio::test]
    async fn mock_workspace_membership_checker_works() {
        let checker = MockWorkspaceMembershipChecker;
        let is_member = checker.is_member(Uuid::nil(), Uuid::nil()).await.unwrap();
        assert!(is_member);
    }

    #[test]
    fn mock_task_heartbeat_works() {
        let tracker = MockTaskHeartbeat;
        tracker.register("test-task", 60);
        tracker.heartbeat("test-task");
        tracker.report_error("test-task", "connection refused");
    }

    #[tokio::test]
    async fn mock_merge_request_handler_works() {
        let handler = MockMergeRequestHandler;
        handler.try_auto_merge(Uuid::nil()).await;
    }

    #[tokio::test]
    async fn mock_ops_repo_manager_works() {
        let mgr = MockOpsRepoManager;
        assert!(
            mgr.read_file(std::path::Path::new("/repos/test"), "main", "values.yaml")
                .await
                .is_none()
        );
        mgr.sync_from_project(Uuid::nil(), std::path::Path::new("/repos/test"), "main")
            .await
            .unwrap();
        let sha = mgr
            .write_file(
                std::path::Path::new("/repos/test"),
                "main",
                "config.yaml",
                b"data",
                "update",
            )
            .await
            .unwrap();
        assert_eq!(sha, "abc123");
        assert!(
            mgr.read_dir_yaml(std::path::Path::new("/repos/test"), "main", "base/")
                .await
                .is_none()
        );
        let sha = mgr
            .commit_values(
                std::path::Path::new("/repos/ops"),
                "main",
                &[("image", "app:v2")],
                "bump",
            )
            .await
            .unwrap();
        assert_eq!(sha, "abc123");
    }

    #[cfg(feature = "kube")]
    #[tokio::test]
    async fn mock_manifest_applier_works() {
        // ManifestApplier requires a kube::Client which needs a real cluster,
        // so we only verify the trait compiles with the mock.
        let _applier = MockManifestApplier;
    }

    #[cfg(feature = "kube")]
    #[tokio::test]
    async fn mock_registry_credential_provider_works() {
        // RegistryCredentialProvider requires a kube::Client, verify trait compiles.
        let _provider = MockRegistryCredentialProvider;
    }
}
