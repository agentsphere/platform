use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TreeEntry {
    pub name: String,
    pub entry_type: String, // "blob" or "tree"
    pub mode: String,
    pub size: Option<i64>,
    pub sha: String,
}

#[derive(Debug, Serialize)]
pub struct BlobResponse {
    pub path: String,
    pub size: i64,
    pub content: String,
    pub encoding: String, // "utf-8" or "base64"
}

#[derive(Debug, Serialize)]
pub struct BranchInfo {
    pub name: String,
    pub sha: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committed_at: String,
}

#[derive(Debug, Deserialize)]
pub struct TreeQuery {
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct BlobQuery {
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct CommitsQuery {
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
    pub limit: Option<i64>,
}

fn default_ref() -> String {
    "HEAD".to_owned()
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/projects/{id}/tree", get(tree))
        .route("/api/projects/{id}/blob", get(blob))
        .route("/api/projects/{id}/branches", get(branches))
        .route("/api/projects/{id}/commits", get(commits))
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

fn validate_git_ref(git_ref: &str) -> Result<(), ApiError> {
    if git_ref.is_empty()
        || git_ref.contains("..")
        || git_ref.contains(';')
        || git_ref.contains('|')
        || git_ref.contains('$')
        || git_ref.contains('`')
        || git_ref.contains('\n')
        || git_ref.contains('\0')
        || git_ref.contains(' ')
    {
        return Err(ApiError::BadRequest("invalid git ref".into()));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), ApiError> {
    if path.contains("..") || path.contains('\0') {
        return Err(ApiError::BadRequest("invalid path".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async fn get_repo_path(
    pool: &PgPool,
    config: &crate::config::Config,
    project_id: Uuid,
) -> Result<(PathBuf, String), ApiError> {
    let project = sqlx::query!(
        "SELECT repo_path, default_branch, owner_id, name FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let repo_path = if let Some(p) = project.repo_path {
        PathBuf::from(p)
    } else {
        // Derive from owner name + project name
        let owner_name = sqlx::query_scalar!(
            r#"SELECT name as "name!" FROM users WHERE id = $1"#,
            project.owner_id,
        )
        .fetch_one(pool)
        .await?;
        config
            .git_repos_path
            .join(owner_name)
            .join(format!("{}.git", project.name))
    };

    Ok((repo_path, project.default_branch))
}

async fn check_project_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectRead,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/projects/:id/tree?ref=main&path=/`
///
/// List directory contents via `git ls-tree`.
#[tracing::instrument(skip(state), fields(%id), err)]
async fn tree(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<Vec<TreeEntry>>, ApiError> {
    check_project_read(&state, &auth, id).await?;
    validate_git_ref(&query.git_ref)?;
    validate_path(&query.path)?;

    let (repo_path, _default_branch) = get_repo_path(&state.pool, &state.config, id).await?;

    // Build the tree-ish argument: ref:path or just ref for root
    let treeish = if query.path.is_empty() || query.path == "/" {
        query.git_ref.clone()
    } else {
        let clean_path = query.path.trim_start_matches('/');
        format!("{}:{clean_path}", query.git_ref)
    };

    let output = tokio::time::timeout(GIT_TIMEOUT, {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .arg("ls-tree")
            .arg("-l") // include size
            .arg("--")
            .arg(&treeish)
            .output()
    })
    .await
    .map_err(|_| ApiError::Internal(anyhow::anyhow!("git ls-tree timed out after 30s")))?
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to run git ls-tree: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") || stderr.contains("not a tree object") {
            return Err(ApiError::NotFound("tree path".into()));
        }
        return Err(ApiError::Internal(anyhow::anyhow!(
            "git ls-tree failed: {stderr}"
        )));
    }

    let entries = parse_ls_tree(&String::from_utf8_lossy(&output.stdout));
    Ok(Json(entries))
}

/// `GET /api/projects/:id/blob?ref=main&path=src/main.rs`
///
/// Read file contents via `git show ref:path`.
#[tracing::instrument(skip(state), fields(%id), err)]
async fn blob(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(query): Query<BlobQuery>,
) -> Result<Json<BlobResponse>, ApiError> {
    check_project_read(&state, &auth, id).await?;
    validate_git_ref(&query.git_ref)?;
    validate_path(&query.path)?;

    if query.path.is_empty() {
        return Err(ApiError::BadRequest("path is required".into()));
    }

    let (repo_path, _default_branch) = get_repo_path(&state.pool, &state.config, id).await?;
    let clean_path = query.path.trim_start_matches('/');
    let object_spec = format!("{}:{clean_path}", query.git_ref);

    let output = tokio::time::timeout(GIT_TIMEOUT, {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .arg("show")
            .arg("--")
            .arg(&object_spec)
            .output()
    })
    .await
    .map_err(|_| ApiError::Internal(anyhow::anyhow!("git show timed out after 30s")))?
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to run git show: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not exist") || stderr.contains("not a valid object") {
            return Err(ApiError::NotFound("blob".into()));
        }
        return Err(ApiError::Internal(anyhow::anyhow!(
            "git show failed: {stderr}"
        )));
    }

    #[allow(clippy::cast_possible_wrap)]
    let size = output.stdout.len() as i64;

    // Try UTF-8 first; fall back to base64 for binary
    let (content, encoding) = match String::from_utf8(output.stdout.clone()) {
        Ok(text) => (text, "utf-8".to_owned()),
        Err(_) => (
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &output.stdout),
            "base64".to_owned(),
        ),
    };

    Ok(Json(BlobResponse {
        path: query.path.clone(),
        size,
        content,
        encoding,
    }))
}

/// `GET /api/projects/:id/branches`
///
/// List branches via `git for-each-ref`.
#[tracing::instrument(skip(state), fields(%id), err)]
async fn branches(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<BranchInfo>>, ApiError> {
    check_project_read(&state, &auth, id).await?;

    let (repo_path, _default_branch) = get_repo_path(&state.pool, &state.config, id).await?;

    let output = tokio::time::timeout(GIT_TIMEOUT, {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .arg("for-each-ref")
            .arg("--format=%(refname:short)\t%(objectname:short)\t%(creatordate:iso-strict)")
            .arg("refs/heads/")
            .output()
    })
    .await
    .map_err(|_| ApiError::Internal(anyhow::anyhow!("git for-each-ref timed out after 30s")))?
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to run git for-each-ref: {e}")))?;

    if !output.status.success() {
        // Empty repo — no branches yet
        return Ok(Json(Vec::new()));
    }

    let branches = parse_branches(&String::from_utf8_lossy(&output.stdout));
    Ok(Json(branches))
}

/// `GET /api/projects/:id/commits?ref=main&limit=20`
///
/// List recent commits via `git log`.
#[tracing::instrument(skip(state), fields(%id), err)]
async fn commits(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(query): Query<CommitsQuery>,
) -> Result<Json<Vec<CommitInfo>>, ApiError> {
    check_project_read(&state, &auth, id).await?;
    validate_git_ref(&query.git_ref)?;

    let (repo_path, _default_branch) = get_repo_path(&state.pool, &state.config, id).await?;

    let limit = query.limit.unwrap_or(20).clamp(1, 100);

    let output = tokio::time::timeout(GIT_TIMEOUT, {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .arg("log")
            .arg(format!("-n{limit}"))
            .arg("--format=%H%x00%s%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI")
            .arg("--")
            .arg(&query.git_ref)
            .output()
    })
    .await
    .map_err(|_| ApiError::Internal(anyhow::anyhow!("git log timed out after 30s")))?
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to run git log: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unknown revision") || stderr.contains("bad default revision") {
            // Empty repo or invalid ref
            return Ok(Json(Vec::new()));
        }
        return Err(ApiError::Internal(anyhow::anyhow!(
            "git log failed: {stderr}"
        )));
    }

    let commits_list = parse_log(&String::from_utf8_lossy(&output.stdout));
    Ok(Json(commits_list))
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Parse `git ls-tree -l` output.
///
/// Format: `<mode> <type> <sha> <size>\t<name>`
/// Size is `-` for trees.
fn parse_ls_tree(output: &str) -> Vec<TreeEntry> {
    output
        .lines()
        .filter_map(|line| {
            let (meta, name) = line.split_once('\t')?;
            let parts: Vec<&str> = meta.split_whitespace().collect();
            if parts.len() < 4 {
                return None;
            }
            let size = parts[3].parse::<i64>().ok();
            Some(TreeEntry {
                mode: parts[0].to_owned(),
                entry_type: parts[1].to_owned(),
                sha: parts[2].to_owned(),
                size,
                name: name.to_owned(),
            })
        })
        .collect()
}

/// Parse `git for-each-ref` output for branches.
///
/// Format: `<name>\t<sha>\t<date>`
fn parse_branches(output: &str) -> Vec<BranchInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let name = parts.next()?.to_owned();
            let sha = parts.next()?.to_owned();
            let updated_at = parts.next().unwrap_or_default().to_owned();
            Some(BranchInfo {
                name,
                sha,
                updated_at,
            })
        })
        .collect()
}

/// Parse `git log` output with null-delimited fields.
///
/// Format per line: `sha\0subject\0author_name\0author_email\0author_date\0committer_name\0committer_email\0committer_date`
fn parse_log(output: &str) -> Vec<CommitInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(8, '\0').collect();
            if parts.len() < 8 {
                return None;
            }
            Some(CommitInfo {
                sha: parts[0].to_owned(),
                message: parts[1].to_owned(),
                author_name: parts[2].to_owned(),
                author_email: parts[3].to_owned(),
                authored_at: parts[4].to_owned(),
                committer_name: parts[5].to_owned(),
                committer_email: parts[6].to_owned(),
                committed_at: parts[7].to_owned(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ref_accepts_valid() {
        assert!(validate_git_ref("main").is_ok());
        assert!(validate_git_ref("feature/foo").is_ok());
        assert!(validate_git_ref("v1.0.0").is_ok());
        assert!(validate_git_ref("HEAD").is_ok());
    }

    #[test]
    fn validate_ref_rejects_invalid() {
        assert!(validate_git_ref("").is_err());
        assert!(validate_git_ref("foo..bar").is_err());
        assert!(validate_git_ref("foo;rm").is_err());
        assert!(validate_git_ref("foo|bar").is_err());
        assert!(validate_git_ref("$HOME").is_err());
    }

    #[test]
    fn validate_path_accepts_valid() {
        assert!(validate_path("src/main.rs").is_ok());
        assert!(validate_path("/").is_ok());
        assert!(validate_path("").is_ok());
    }

    #[test]
    fn validate_path_rejects_traversal() {
        assert!(validate_path("../etc/passwd").is_err());
        assert!(validate_path("foo/../../bar").is_err());
    }

    #[test]
    fn parse_ls_tree_normal() {
        let output = "100644 blob abc1234 1234\tREADME.md\n040000 tree def5678      -\tsrc\n";
        let entries = parse_ls_tree(output);
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].name, "README.md");
        assert_eq!(entries[0].entry_type, "blob");
        assert_eq!(entries[0].size, Some(1234));

        assert_eq!(entries[1].name, "src");
        assert_eq!(entries[1].entry_type, "tree");
        assert_eq!(entries[1].size, None);
    }

    #[test]
    fn parse_ls_tree_empty() {
        assert!(parse_ls_tree("").is_empty());
    }

    #[test]
    fn parse_branches_normal() {
        let output = "main\tabc1234\t2026-02-19T10:00:00+00:00\nfeature\tdef5678\t2026-02-18T09:00:00+00:00\n";
        let branches = parse_branches(output);
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].name, "main");
        assert_eq!(branches[1].name, "feature");
    }

    #[test]
    fn parse_log_normal() {
        let output = "abc123\0Initial commit\0Alice\0alice@example.com\02026-02-19T10:00:00+00:00\0Alice\0alice@example.com\02026-02-19T10:00:00+00:00\n";
        let commits_list = parse_log(output);
        assert_eq!(commits_list.len(), 1);
        assert_eq!(commits_list[0].sha, "abc123");
        assert_eq!(commits_list[0].message, "Initial commit");
        assert_eq!(commits_list[0].author_name, "Alice");
    }

    #[test]
    fn parse_log_empty() {
        assert!(parse_log("").is_empty());
    }
}
