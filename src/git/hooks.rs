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
    /// Branch names that were updated (stripped of `refs/heads/` prefix).
    /// When empty, falls back to `default_branch`.
    pub pushed_branches: Vec<String>,
    /// Tag names that were pushed (stripped of `refs/tags/` prefix).
    pub pushed_tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Extract branch names from ref updates, filtering to `refs/heads/*` only.
///
/// Strips the `refs/heads/` prefix and skips deletions (`new_sha` all zeros)
/// and non-branch refs (tags, etc.).
pub fn extract_pushed_branches(updates: &[RefUpdate]) -> Vec<String> {
    let zero_sha = "0".repeat(40);
    updates
        .iter()
        .filter_map(|u| {
            // Skip deletions
            if u.new_sha == zero_sha {
                return None;
            }
            // Only process branch refs
            u.refname.strip_prefix("refs/heads/").map(str::to_string)
        })
        .collect()
}

/// Extract tag names from ref updates, filtering to `refs/tags/*` only.
///
/// Strips the `refs/tags/` prefix and skips deletions.
pub fn extract_pushed_tags(updates: &[RefUpdate]) -> Vec<String> {
    let zero_sha = "0".repeat(40);
    updates
        .iter()
        .filter_map(|u| {
            if u.new_sha == zero_sha {
                return None;
            }
            u.refname.strip_prefix("refs/tags/").map(str::to_string)
        })
        .collect()
}

/// Parse ref update commands from a git receive-pack request body (pkt-line format).
///
/// The pack data starts with pkt-line encoded ref commands:
/// ```text
/// <4-hex-len><old-sha> <new-sha> <refname>\0<capabilities>\n
/// <4-hex-len><old-sha> <new-sha> <refname>\n
/// 0000
/// PACK...
/// ```
pub fn parse_pack_commands(data: &[u8]) -> Vec<RefUpdate> {
    let mut updates = Vec::new();
    let mut pos = 0;

    while pos + 4 <= data.len() {
        let Ok(len_hex) = std::str::from_utf8(&data[pos..pos + 4]) else {
            break;
        };

        // "0000" marks end of commands
        if len_hex == "0000" {
            break;
        }

        let pkt_len = match usize::from_str_radix(len_hex, 16) {
            Ok(n) if n >= 4 => n,
            _ => break,
        };

        if pos + pkt_len > data.len() {
            break;
        }

        // Extract the data portion (after the 4-byte length prefix)
        let line_bytes = &data[pos + 4..pos + pkt_len];

        // Convert to string, strip NUL and everything after (capabilities)
        if let Ok(line) = std::str::from_utf8(line_bytes) {
            let line = line.split('\0').next().unwrap_or(line).trim();
            let mut parts = line.splitn(3, ' ');
            if let (Some(old_sha), Some(new_sha), Some(refname)) =
                (parts.next(), parts.next(), parts.next())
                && old_sha.len() >= 40
                && new_sha.len() >= 40
                && !refname.is_empty()
            {
                updates.push(RefUpdate {
                    old_sha: old_sha.to_owned(),
                    new_sha: new_sha.to_owned(),
                    refname: refname.to_owned(),
                });
            }
        }

        pos += pkt_len;
    }

    updates
}

/// Parse ref update lines from receive-pack output.
///
/// Each line has the format: `old_sha new_sha refname\n`
#[cfg(test)]
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
/// For each pushed branch:
/// 1. Delegate to `pipeline::trigger::on_push()` to parse `.platform.yaml` and create pipeline + steps
/// 2. If a pipeline was created, notify the executor via Valkey
/// 3. Fire push webhooks
#[tracing::instrument(skip(state, params), fields(project_id = %params.project_id, user = %params.user_name), err)]
pub async fn post_receive(state: &AppState, params: &PostReceiveParams) -> Result<(), ApiError> {
    // Use pushed branches if available, otherwise fall back to default branch
    let branches: Vec<&str> = if params.pushed_branches.is_empty() {
        vec![params.default_branch.as_str()]
    } else {
        params.pushed_branches.iter().map(String::as_str).collect()
    };

    let mut pipelines_triggered = 0u32;
    for branch in &branches {
        tracing::info!(branch, "post-receive: processing push");
        let commit_sha = get_branch_sha(&params.repo_path, branch).await;

        let trigger_params = crate::pipeline::trigger::PushTriggerParams {
            project_id: params.project_id,
            user_id: params.user_id,
            repo_path: params.repo_path.clone(),
            branch: (*branch).to_string(),
            commit_sha,
        };

        match crate::pipeline::trigger::on_push(&state.pool, &trigger_params).await {
            Ok(Some(pipeline_id)) => {
                tracing::info!(%pipeline_id, branch, "pipeline created, notifying executor");
                crate::pipeline::trigger::notify_executor(state, pipeline_id).await;
                pipelines_triggered += 1;
            }
            Ok(None) => {
                tracing::info!(branch, "no pipeline triggered for branch");
            }
            Err(e) => {
                tracing::error!(error = %e, branch, "pipeline trigger failed");
            }
        }
    }
    tracing::info!(
        branches = branches.len(),
        pipelines_triggered,
        "post-receive complete"
    );

    // Fire push webhooks for each pushed branch
    for branch in &branches {
        let payload = serde_json::json!({
            "ref": format!("refs/heads/{branch}"),
            "project_id": params.project_id,
            "pusher": params.user_name,
        });
        crate::api::webhooks::fire_webhooks(&state.pool, params.project_id, "push", &payload).await;
    }

    // Handle MR sync on push: update head_sha and trigger MR pipelines
    for branch in &branches {
        handle_mr_sync_on_push(state, params, branch).await;
    }

    // Handle tag pushes
    for tag_name in &params.pushed_tags {
        let commit_sha = get_tag_sha(&params.repo_path, tag_name).await;
        let tag_params = crate::pipeline::trigger::TagTriggerParams {
            project_id: params.project_id,
            user_id: params.user_id,
            repo_path: params.repo_path.clone(),
            tag_name: tag_name.clone(),
            commit_sha: commit_sha.clone(),
        };

        match crate::pipeline::trigger::on_tag(&state.pool, &tag_params).await {
            Ok(Some(pipeline_id)) => {
                crate::pipeline::trigger::notify_executor(state, pipeline_id).await;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, %tag_name, "tag pipeline trigger failed");
            }
        }

        let payload = serde_json::json!({
            "ref": format!("refs/tags/{tag_name}"),
            "project_id": params.project_id,
            "pusher": params.user_name,
        });
        crate::api::webhooks::fire_webhooks(&state.pool, params.project_id, "push", &payload).await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Handle MR sync when a branch is pushed: update `head_sha`, dismiss stale reviews, trigger MR pipeline.
async fn handle_mr_sync_on_push(state: &AppState, params: &PostReceiveParams, branch: &str) {
    let commit_sha = get_branch_sha(&params.repo_path, branch).await;
    let sha_str = commit_sha.as_deref().unwrap_or("");

    // Find open MRs where source_branch matches
    let open_mrs = sqlx::query!(
        r#"
        SELECT id, number FROM merge_requests
        WHERE project_id = $1 AND source_branch = $2 AND status = 'open'
        "#,
        params.project_id,
        branch,
    )
    .fetch_all(&state.pool)
    .await;

    let Ok(open_mrs) = open_mrs else {
        return;
    };

    for mr in open_mrs {
        // Update head_sha
        let _ = sqlx::query!(
            "UPDATE merge_requests SET head_sha = $1, updated_at = now() WHERE id = $2",
            sha_str,
            mr.id,
        )
        .execute(&state.pool)
        .await;

        // Dismiss stale reviews if protection rule says so
        if let Ok(Some(rule)) =
            crate::git::protection::get_protection(&state.pool, params.project_id, branch).await
            && rule.dismiss_stale_reviews
        {
            let _ = sqlx::query!(
                r#"UPDATE mr_reviews SET is_stale = true
                    WHERE mr_id = $1 AND verdict = 'approve' AND is_stale = false"#,
                mr.id,
            )
            .execute(&state.pool)
            .await;
        }

        // Trigger MR pipeline
        let mr_params = crate::pipeline::trigger::MrTriggerParams {
            project_id: params.project_id,
            user_id: params.user_id,
            repo_path: params.repo_path.clone(),
            source_branch: branch.to_string(),
            commit_sha: commit_sha.clone(),
            action: "synchronized".into(),
        };

        match crate::pipeline::trigger::on_mr(&state.pool, &mr_params).await {
            Ok(Some(pipeline_id)) => {
                crate::pipeline::trigger::notify_executor(state, pipeline_id).await;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, mr_number = mr.number, "MR pipeline trigger on push failed");
            }
        }
    }
}

/// Get the SHA of a tag.
async fn get_tag_sha(repo_path: &Path, tag_name: &str) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg(format!("refs/tags/{tag_name}"))
        .output()
        .await
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

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

    #[test]
    fn parse_mixed_valid_and_invalid() {
        let input = "\
invalid line
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/main
too few parts
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd refs/heads/feature
";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].refname, "refs/heads/main");
        assert_eq!(updates[1].refname, "refs/heads/feature");
    }

    #[test]
    fn parse_tag_ref() {
        let input = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/tags/v1.0.0\n";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].refname, "refs/tags/v1.0.0");
    }

    #[test]
    fn parse_whitespace_trimmed() {
        let input = "  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/main  \n";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn parse_refname_with_slashes() {
        let input = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/feature/deep/nested/branch\n";
        let updates = parse_ref_updates(input);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].refname, "refs/heads/feature/deep/nested/branch");
    }

    #[test]
    fn parse_exactly_40_char_sha() {
        // Exactly 40 chars should pass
        let sha = "a".repeat(40);
        let input = format!("{sha} {sha} refs/heads/main\n");
        let updates = parse_ref_updates(&input);
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn parse_39_char_sha_rejected() {
        let sha = "a".repeat(39);
        let input = format!("{sha} {sha} refs/heads/main\n");
        let updates = parse_ref_updates(&input);
        assert_eq!(updates.len(), 0);
    }

    #[test]
    fn parse_longer_sha_accepted() {
        // 64 chars should pass (SHA-256 format)
        let sha = "a".repeat(64);
        let input = format!("{sha} {sha} refs/heads/main\n");
        let updates = parse_ref_updates(&input);
        assert_eq!(updates.len(), 1);
    }

    #[tokio::test]
    async fn get_branch_sha_nonexistent_repo() {
        let result = get_branch_sha(Path::new("/nonexistent/repo.git"), "main").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn check_file_exists_nonexistent_repo() {
        let result =
            check_file_exists(Path::new("/nonexistent/repo.git"), "HEAD", "README.md").await;
        assert!(!result);
    }

    #[test]
    fn ref_update_struct_equality() {
        let a = RefUpdate {
            old_sha: "a".repeat(40),
            new_sha: "b".repeat(40),
            refname: "refs/heads/main".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn ref_update_struct_debug() {
        let update = RefUpdate {
            old_sha: "a".repeat(40),
            new_sha: "b".repeat(40),
            refname: "refs/heads/main".into(),
        };
        let debug = format!("{update:?}");
        assert!(debug.contains("RefUpdate"));
        assert!(debug.contains("refs/heads/main"));
    }

    // -- extract_pushed_branches --

    #[test]
    fn extract_branches_from_updates() {
        let updates = vec![
            RefUpdate {
                old_sha: "a".repeat(40),
                new_sha: "b".repeat(40),
                refname: "refs/heads/main".into(),
            },
            RefUpdate {
                old_sha: "a".repeat(40),
                new_sha: "b".repeat(40),
                refname: "refs/heads/feature/login".into(),
            },
        ];
        let branches = extract_pushed_branches(&updates);
        assert_eq!(branches, vec!["main", "feature/login"]);
    }

    #[test]
    fn extract_branches_skips_deletions() {
        let updates = vec![RefUpdate {
            old_sha: "a".repeat(40),
            new_sha: "0".repeat(40),
            refname: "refs/heads/old-branch".into(),
        }];
        let branches = extract_pushed_branches(&updates);
        assert!(branches.is_empty());
    }

    #[test]
    fn extract_branches_skips_tags() {
        let updates = vec![RefUpdate {
            old_sha: "a".repeat(40),
            new_sha: "b".repeat(40),
            refname: "refs/tags/v1.0.0".into(),
        }];
        let branches = extract_pushed_branches(&updates);
        assert!(branches.is_empty());
    }

    // -- parse_pack_commands --

    #[test]
    fn parse_pack_single_ref() {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        let cmd = format!("{old} {new} refs/heads/main\0 report-status\n");
        let pkt_len = cmd.len() + 4;
        let data = format!("{pkt_len:04x}{cmd}0000");
        let updates = parse_pack_commands(data.as_bytes());
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].refname, "refs/heads/main");
        assert_eq!(updates[0].old_sha, old);
        assert_eq!(updates[0].new_sha, new);
    }

    #[test]
    fn parse_pack_multiple_refs() {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        let cmd1 = format!("{old} {new} refs/heads/main\0 report-status\n");
        let cmd2 = format!("{old} {new} refs/heads/feature\n");
        let len1 = cmd1.len() + 4;
        let len2 = cmd2.len() + 4;
        let data = format!("{len1:04x}{cmd1}{len2:04x}{cmd2}0000");
        let updates = parse_pack_commands(data.as_bytes());
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].refname, "refs/heads/main");
        assert_eq!(updates[1].refname, "refs/heads/feature");
    }

    #[test]
    fn parse_pack_empty_data() {
        assert!(parse_pack_commands(b"0000").is_empty());
        assert!(parse_pack_commands(b"").is_empty());
    }

    #[test]
    fn parse_pack_with_trailing_pack_data() {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        let cmd = format!("{old} {new} refs/heads/main\n");
        let pkt_len = cmd.len() + 4;
        let mut data = format!("{pkt_len:04x}{cmd}0000PACK").into_bytes();
        // Append some binary pack data
        data.extend_from_slice(&[0x00, 0x01, 0x02, 0xff]);
        let updates = parse_pack_commands(&data);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].refname, "refs/heads/main");
    }
}
