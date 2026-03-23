//! Preview deployment helpers.
//!
//! Preview reconciliation is handled by the unified reconciler
//! (`reconciler::cleanup_expired_previews`). This module retains:
//! - `stop_preview_for_branch()` — called from MR merge to deactivate previews
//! - Builder functions — used by the unified reconciler for preview K8s resources

use uuid::Uuid;

/// Stop a preview deployment for a given project and branch slug.
/// Called when an MR is merged to clean up the preview for the source branch.
/// Uses `deploy_targets` table (`preview_deployments` was dropped).
pub async fn stop_preview_for_branch(pool: &sqlx::PgPool, project_id: Uuid, branch: &str) {
    let slug = crate::pipeline::slugify_branch(branch);

    // Deactivate the preview target
    let _ = sqlx::query(
        "UPDATE deploy_targets SET is_active = false
         WHERE project_id = $1 AND branch_slug = $2 AND environment = 'preview' AND is_active = true",
    )
    .bind(project_id)
    .bind(&slug)
    .execute(pool)
    .await;

    // Cancel any active releases for that target
    let _ = sqlx::query(
        "UPDATE deploy_releases SET phase = 'cancelled'
         WHERE project_id = $1
           AND target_id IN (SELECT id FROM deploy_targets WHERE project_id = $1 AND branch_slug = $2 AND environment = 'preview')
           AND phase NOT IN ('completed','rolled_back','cancelled','failed')",
    )
    .bind(project_id)
    .bind(&slug)
    .execute(pool)
    .await;

    tracing::info!(
        %project_id,
        branch = %branch,
        branch_slug = %slug,
        "preview deployment stopped"
    );
}

/// Build the K8s namespace name for a preview, respecting the 63-char DNS label limit.
#[allow(dead_code)]
pub fn build_namespace_name(
    config: &crate::config::Config,
    project_slug: &str,
    branch_slug: &str,
) -> String {
    let suffix = format!("preview-{branch_slug}");
    let raw = config.project_namespace(project_slug, &suffix);
    if raw.len() > 63 {
        raw[..63].trim_end_matches('-').to_string()
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> crate::config::Config {
        crate::config::Config::test_default()
    }

    #[test]
    fn build_namespace_name_basic() {
        let config = test_config();
        let name = build_namespace_name(&config, "my-app", "feat-123");
        assert_eq!(name, "my-app-preview-feat-123");
    }

    #[test]
    fn build_namespace_name_long_truncated_to_63() {
        let config = test_config();
        let long_slug = "a".repeat(60);
        let name = build_namespace_name(&config, "my-app", &long_slug);
        assert!(name.len() <= 63);
    }

    #[test]
    fn build_namespace_name_no_trailing_hyphen() {
        let config = test_config();
        // Pick a length that would result in a trailing hyphen after truncation
        let slug = format!("feat-{}", "x".repeat(50));
        let name = build_namespace_name(&config, "my-app", &slug);
        assert!(!name.ends_with('-'));
    }
}
