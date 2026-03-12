use std::path::Path;

use sqlx::PgPool;
use uuid::Uuid;

use crate::validation::match_glob_pattern;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A branch protection rule for a project.
#[allow(clippy::struct_excessive_bools)]
#[allow(dead_code)] // id/project_id/pattern used for debugging and future API needs
#[derive(Debug, Clone)]
pub struct BranchProtection {
    pub id: Uuid,
    pub project_id: Uuid,
    pub pattern: String,
    pub require_pr: bool,
    pub block_force_push: bool,
    pub required_approvals: i32,
    pub dismiss_stale_reviews: bool,
    pub required_checks: Vec<String>,
    pub require_up_to_date: bool,
    pub allow_admin_bypass: bool,
    pub merge_methods: Vec<String>,
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Get the first matching branch protection rule for a branch.
///
/// Returns `None` if no rule matches.
pub async fn get_protection(
    pool: &PgPool,
    project_id: Uuid,
    branch: &str,
) -> Result<Option<BranchProtection>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, pattern, require_pr, block_force_push,
               required_approvals, dismiss_stale_reviews, required_checks,
               require_up_to_date, allow_admin_bypass, merge_methods
        FROM branch_protection_rules
        WHERE project_id = $1
        ORDER BY created_at ASC
        "#,
        project_id,
    )
    .fetch_all(pool)
    .await?;

    for row in rows {
        if match_glob_pattern(&row.pattern, branch) {
            return Ok(Some(BranchProtection {
                id: row.id,
                project_id: row.project_id,
                pattern: row.pattern,
                require_pr: row.require_pr,
                block_force_push: row.block_force_push,
                required_approvals: row.required_approvals,
                dismiss_stale_reviews: row.dismiss_stale_reviews,
                required_checks: row.required_checks,
                require_up_to_date: row.require_up_to_date,
                allow_admin_bypass: row.allow_admin_bypass,
                merge_methods: row.merge_methods,
            }));
        }
    }

    Ok(None)
}

/// Check if a push is a force push (non-fast-forward) by testing if `old_sha`
/// is an ancestor of `new_sha`.
pub async fn is_force_push(repo_path: &Path, old_sha: &str, new_sha: &str) -> bool {
    let zero_sha = "0".repeat(40);
    // Branch creation or deletion is never a force push
    if old_sha == zero_sha || new_sha == zero_sha {
        return false;
    }

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(old_sha)
        .arg(new_sha)
        .output()
        .await;

    match output {
        // exit 0 = is ancestor (fast-forward), exit 1 = not ancestor (force push)
        // exit 128+ = git error (repo missing, bad refs) — don't block
        Ok(o) => o.status.code() == Some(1),
        Err(_) => false, // If git fails, don't block the push
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn force_push_branch_creation_is_not_force() {
        let zero = "0".repeat(40);
        let sha = "a".repeat(40);
        assert!(!is_force_push(Path::new("/nonexistent"), &zero, &sha).await);
    }

    #[tokio::test]
    async fn force_push_branch_deletion_is_not_force() {
        let zero = "0".repeat(40);
        let sha = "a".repeat(40);
        assert!(!is_force_push(Path::new("/nonexistent"), &sha, &zero).await);
    }

    #[tokio::test]
    async fn force_push_nonexistent_repo_returns_false() {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        // Git will fail, which defaults to not blocking
        assert!(!is_force_push(Path::new("/nonexistent/repo.git"), &old, &new).await);
    }
}
