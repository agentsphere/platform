use std::path::Path;

use uuid::Uuid;

use crate::error::ApiError;
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single ref update from a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub old_sha: String,
    pub new_sha: String,
    pub refname: String,
}

/// Parameters for post-receive processing.
pub struct PostReceiveParams {
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub user_name: String,
    pub repo_path: std::path::PathBuf,
    pub default_branch: String,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse ref update lines from receive-pack output.
///
/// Each line has the format: `old_sha new_sha refname\n`
#[allow(dead_code)] // used in tests; will be used in integration wiring for ref-level triggers
pub fn parse_ref_updates(input: &str) -> Vec<RefUpdate> {
    input
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(3, ' ');
            let old_sha = parts.next()?.to_owned();
            let new_sha = parts.next()?.to_owned();
            let refname = parts.next()?.to_owned();
            if old_sha.len() < 40 || new_sha.len() < 40 || refname.is_empty() {
                return None;
            }
            Some(RefUpdate {
                old_sha,
                new_sha,
                refname,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Post-receive processing
// ---------------------------------------------------------------------------

/// Run post-receive logic after a successful push.
///
/// 1. Delegate to `pipeline::trigger::on_push()` to parse `.platform.yaml` and create pipeline + steps
/// 2. If a pipeline was created, notify the executor via Valkey
/// 3. Fire push webhooks
#[tracing::instrument(skip(state, params), fields(project_id = %params.project_id, user = %params.user_name), err)]
pub async fn post_receive(state: &AppState, params: &PostReceiveParams) -> Result<(), ApiError> {
    let commit_sha = get_branch_sha(&params.repo_path, &params.default_branch).await;

    let trigger_params = crate::pipeline::trigger::PushTriggerParams {
        project_id: params.project_id,
        user_id: params.user_id,
        repo_path: params.repo_path.clone(),
        branch: params.default_branch.clone(),
        commit_sha,
    };

    match crate::pipeline::trigger::on_push(&state.pool, &trigger_params).await {
        Ok(Some(pipeline_id)) => {
            crate::pipeline::trigger::notify_executor(&state, pipeline_id).await;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!(error = %e, "pipeline trigger failed");
        }
    }

    // Fire push webhooks
    let payload = serde_json::json!({
        "ref": format!("refs/heads/{}", params.default_branch),
        "project_id": params.project_id,
        "pusher": params.user_name,
    });
    crate::api::webhooks::fire_webhooks(&state.pool, params.project_id, "push", &payload).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Check if a file exists in a git repo at a given ref.
#[allow(dead_code)] // available for future use; trigger module uses read_file_at_ref instead
async fn check_file_exists(repo_path: &Path, git_ref: &str, file_path: &str) -> bool {
    let result = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("cat-file")
        .arg("-e")
        .arg(format!("{git_ref}:{file_path}"))
        .output()
        .await;

    matches!(result, Ok(output) if output.status.success())
}

/// Get the SHA of a branch tip.
async fn get_branch_sha(repo_path: &Path, branch: &str) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normal_push() {
        let input = "abc123abc123abc123abc123abc123abc123abc12a def456def456def456def456def456def456def456d refs/heads/main\n";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].refname, "refs/heads/main");
    }

    #[test]
    fn parse_multiple_refs() {
        let input = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/main
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd refs/heads/feature
";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].refname, "refs/heads/main");
        assert_eq!(updates[1].refname, "refs/heads/feature");
    }

    #[test]
    fn parse_branch_create() {
        let input = "0000000000000000000000000000000000000000 abcdef1234567890abcdef1234567890abcdef12 refs/heads/new-branch\n";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].old_sha,
            "0000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn parse_branch_delete() {
        let input = "abcdef1234567890abcdef1234567890abcdef12 0000000000000000000000000000000000000000 refs/heads/old-branch\n";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].new_sha,
            "0000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn parse_empty_input() {
        assert!(parse_ref_updates("").is_empty());
        assert!(parse_ref_updates("  \n  \n").is_empty());
    }

    #[test]
    fn parse_malformed_lines() {
        // Too few parts
        assert!(parse_ref_updates("abc123 refs/heads/main").is_empty());
        // SHA too short
        assert!(parse_ref_updates("short short refs/heads/main").is_empty());
    }
}
