use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

use ts_rs::TS;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::git::signature::{self, SignatureInfo, SignatureStatus};
use crate::rbac::{Permission, resolver};
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct TreeEntry {
    pub name: String,
    pub entry_type: String, // "blob" or "tree"
    pub mode: String,
    #[ts(type = "number | null")]
    pub size: Option<i64>,
    pub sha: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct BlobResponse {
    pub path: String,
    #[ts(type = "number")]
    pub size: i64,
    pub content: String,
    pub encoding: String, // "utf-8" or "base64"
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct BranchInfo {
    pub name: String,
    pub sha: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub signature: Option<SignatureInfo>,
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
    #[serde(default)]
    pub verify_signatures: bool,
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
        .route("/api/projects/{id}/commits/{sha}", get(commit_detail))
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
    auth.check_project_scope(project_id)?;
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectRead,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::NotFound("project".into()));
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

    // A15: Reject oversized blobs before converting to string/base64
    const MAX_BLOB_SIZE: usize = 50 * 1024 * 1024; // 50 MB
    if output.stdout.len() > MAX_BLOB_SIZE {
        return Err(ApiError::BadRequest(format!(
            "file too large: {} bytes (max {MAX_BLOB_SIZE})",
            output.stdout.len()
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
            .arg(&query.git_ref)
            .arg("--")
            .output()
    })
    .await
    .map_err(|_| ApiError::Internal(anyhow::anyhow!("git log timed out after 30s")))?
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to run git log: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unknown revision")
            || stderr.contains("bad default revision")
            || stderr.contains("bad revision")
        {
            // Empty repo or invalid ref (git says "bad revision 'HEAD'" on empty repos)
            return Ok(Json(Vec::new()));
        }
        return Err(ApiError::Internal(anyhow::anyhow!(
            "git log failed: {stderr}"
        )));
    }

    let mut commits_list = parse_log(&String::from_utf8_lossy(&output.stdout));

    if query.verify_signatures {
        let (repo_path_clone, pool_clone, valkey_clone) =
            (repo_path.clone(), state.pool.clone(), state.valkey.clone());
        let shas: Vec<String> = commits_list.iter().map(|c| c.sha.clone()).collect();
        let sigs =
            verify_commits_batch(&repo_path_clone, &pool_clone, &valkey_clone, id, &shas).await;
        for (commit, sig) in commits_list.iter_mut().zip(sigs) {
            commit.signature = Some(sig);
        }
    }

    Ok(Json(commits_list))
}

/// `GET /api/projects/:id/commits/:sha`
///
/// Single commit detail with signature verification.
#[tracing::instrument(skip(state), fields(%id, %sha), err)]
async fn commit_detail(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, sha)): Path<(Uuid, String)>,
) -> Result<Json<CommitInfo>, ApiError> {
    check_project_read(&state, &auth, id).await?;

    if !signature::validate_commit_sha(&sha) {
        return Err(ApiError::BadRequest("invalid commit SHA".into()));
    }

    let (repo_path, _default_branch) = get_repo_path(&state.pool, &state.config, id).await?;

    // Get the full commit info via git log -1
    let output = tokio::time::timeout(GIT_TIMEOUT, {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .arg("log")
            .arg("-n1")
            .arg("--format=%H%x00%s%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI")
            .arg(&sha)
            .arg("--")
            .output()
    })
    .await
    .map_err(|_| ApiError::Internal(anyhow::anyhow!("git log timed out after 30s")))?
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to run git log: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unknown revision")
            || stderr.contains("bad default revision")
            || stderr.contains("bad object")
        {
            return Err(ApiError::NotFound("commit".into()));
        }
        return Err(ApiError::Internal(anyhow::anyhow!(
            "git log failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits_list = parse_log(&stdout);
    if commits_list.is_empty() {
        return Err(ApiError::NotFound("commit".into()));
    }

    let mut commit = commits_list.remove(0);

    // Always verify signature for single commit detail
    commit.signature =
        Some(verify_single_commit(&repo_path, &state.pool, &state.valkey, id, &commit.sha).await);

    Ok(Json(commit))
}

// ---------------------------------------------------------------------------
// Signature verification
// ---------------------------------------------------------------------------

/// Verify a single commit's signature.
async fn verify_single_commit(
    repo_path: &std::path::Path,
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    project_id: Uuid,
    sha: &str,
) -> SignatureInfo {
    use fred::interfaces::KeysInterface;

    // Check cache first
    let cache_key = format!("gpg:sig:{project_id}:{sha}");
    if let Ok(Some(cached_json)) = valkey.get::<Option<String>, _>(&cache_key).await
        && let Ok(info) = serde_json::from_str::<SignatureInfo>(&cached_json)
    {
        return info;
    }

    let info = do_verify_commit(repo_path, pool, sha).await;

    // Cache the result (5 min TTL — short to limit stale results after key deletion)
    if let Ok(json) = serde_json::to_string(&info) {
        let _: Result<(), _> = valkey
            .set::<(), _, _>(&cache_key, json.as_str(), None, None, false)
            .await;
        let _: Result<(), _> = valkey.expire::<(), _>(&cache_key, 300, None).await;
    }

    info
}

/// Perform the actual signature verification against the git repo and database.
async fn do_verify_commit(repo_path: &std::path::Path, pool: &PgPool, sha: &str) -> SignatureInfo {
    let Some(raw_commit) = git_cat_file_commit(repo_path, sha).await else {
        return no_signature();
    };

    let Some(parsed) = signature::parse_commit_gpgsig(&raw_commit) else {
        return no_signature();
    };

    let Some(key_id) = signature::extract_signing_key_id(&parsed.signature_armor) else {
        return bad_signature(None, None);
    };

    let Some(row) = lookup_gpg_key(pool, &key_id).await else {
        return bad_signature(Some(key_id), None);
    };

    verify_against_key(&parsed, &raw_commit, &key_id, row).await
}

/// Run `git cat-file commit <sha>` and return the raw output.
async fn git_cat_file_commit(repo_path: &std::path::Path, sha: &str) -> Option<Vec<u8>> {
    let result = tokio::time::timeout(GIT_TIMEOUT, {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .arg("cat-file")
            .arg("commit")
            .arg(sha)
            .output()
    })
    .await;

    match result {
        Ok(Ok(out)) if out.status.success() => Some(out.stdout),
        _ => None,
    }
}

/// Look up a GPG key in the database by key ID.
async fn lookup_gpg_key(pool: &PgPool, key_id: &str) -> Option<GpgKeyRow> {
    let result = sqlx::query!(
        r#"SELECT public_key_bytes, fingerprint, emails
           FROM user_gpg_keys
           WHERE key_id = $1
             AND can_sign = true
             AND (expires_at IS NULL OR expires_at > now())"#,
        key_id,
    )
    .fetch_optional(pool)
    .await;

    match result {
        Ok(Some(row)) => Some(GpgKeyRow {
            public_key_bytes: row.public_key_bytes,
            fingerprint: row.fingerprint,
            emails: row.emails,
        }),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, key_id = %key_id, "GPG key lookup failed");
            None
        }
    }
}

struct GpgKeyRow {
    public_key_bytes: Vec<u8>,
    fingerprint: String,
    emails: Vec<String>,
}

/// Verify the commit signature against a stored GPG key.
async fn verify_against_key(
    parsed: &signature::ParsedCommitSignature,
    raw_commit: &[u8],
    key_id: &str,
    row: GpgKeyRow,
) -> SignatureInfo {
    use pgp::composed::{Deserializable, SignedPublicKey};

    let Ok(public_key) = SignedPublicKey::from_bytes(std::io::Cursor::new(&row.public_key_bytes))
    else {
        return bad_signature(Some(key_id.to_owned()), Some(row.fingerprint));
    };

    let sig_armor = parsed.signature_armor.clone();
    let signed_data = parsed.signed_data.clone();
    let pk = public_key.clone();
    let valid = tokio::task::spawn_blocking(move || {
        signature::verify_signature(&sig_armor, &signed_data, &pk)
    })
    .await
    .unwrap_or(false);

    if !valid {
        return bad_signature(Some(key_id.to_owned()), Some(row.fingerprint));
    }

    let signer_name = public_key
        .details
        .users
        .first()
        .map(|u| u.id.id().to_string());

    let author_email = extract_author_email_from_commit(raw_commit);
    let email_match = author_email
        .as_ref()
        .is_some_and(|email| row.emails.iter().any(|ke| ke.eq_ignore_ascii_case(email)));

    let status = if email_match {
        SignatureStatus::Verified
    } else {
        SignatureStatus::UnverifiedSigner
    };

    SignatureInfo {
        status,
        signer_key_id: Some(key_id.to_owned()),
        signer_fingerprint: Some(row.fingerprint),
        signer_name,
    }
}

fn no_signature() -> SignatureInfo {
    SignatureInfo {
        status: SignatureStatus::NoSignature,
        signer_key_id: None,
        signer_fingerprint: None,
        signer_name: None,
    }
}

fn bad_signature(key_id: Option<String>, fingerprint: Option<String>) -> SignatureInfo {
    SignatureInfo {
        status: SignatureStatus::BadSignature,
        signer_key_id: key_id,
        signer_fingerprint: fingerprint,
        signer_name: None,
    }
}

/// Extract the author email from a raw commit object.
pub fn extract_author_email_from_commit(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("author ")
            && let Some(start) = rest.find('<')
            && let Some(end) = rest[start..].find('>')
        {
            return Some(rest[start + 1..start + end].to_owned());
        }
    }
    None
}

/// Verify signatures for a batch of commits in parallel.
async fn verify_commits_batch(
    repo_path: &std::path::Path,
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    project_id: Uuid,
    shas: &[String],
) -> Vec<SignatureInfo> {
    let futures: Vec<_> = shas
        .iter()
        .map(|sha| verify_single_commit(repo_path, pool, valkey, project_id, sha))
        .collect();
    futures_util::future::join_all(futures).await
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
                signature: None,
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
        let output = "abc123\0Initial commit\0Alice\0alice@example.com\x002026-02-19T10:00:00+00:00\0Alice\0alice@example.com\x002026-02-19T10:00:00+00:00\n";
        let commits_list = parse_log(output);
        assert_eq!(commits_list.len(), 1);
        assert_eq!(commits_list[0].sha, "abc123");
        assert_eq!(commits_list[0].message, "Initial commit");
        assert_eq!(commits_list[0].author_name, "Alice");
        assert!(commits_list[0].signature.is_none());
    }

    #[test]
    fn parse_log_empty() {
        assert!(parse_log("").is_empty());
    }

    #[test]
    fn commits_query_verify_signatures_default_false() {
        let query: CommitsQuery =
            serde_json::from_value(serde_json::json!({"ref": "main", "limit": 10})).unwrap();
        assert!(!query.verify_signatures);
    }

    #[test]
    fn commits_query_verify_signatures_true() {
        let query: CommitsQuery =
            serde_json::from_value(serde_json::json!({"ref": "main", "verify_signatures": true}))
                .unwrap();
        assert!(query.verify_signatures);
    }

    // -- extract_author_email_from_commit --

    #[test]
    fn extract_author_email_standard_format() {
        let raw = b"tree abc123\nauthor Alice <alice@example.com> 1700000000 +0000\ncommitter Bob <bob@example.com> 1700000000 +0000\n\ncommit message\n";
        let email = extract_author_email_from_commit(raw);
        assert_eq!(email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn extract_author_email_no_author_line() {
        let raw =
            b"tree abc123\ncommitter Bob <bob@example.com> 1700000000 +0000\n\ncommit message\n";
        let email = extract_author_email_from_commit(raw);
        assert!(email.is_none(), "no author line should return None");
    }

    #[test]
    fn extract_author_email_no_angle_brackets() {
        let raw = b"tree abc123\nauthor Alice 1700000000 +0000\n\ncommit message\n";
        let email = extract_author_email_from_commit(raw);
        assert!(email.is_none(), "malformed author should return None");
    }

    #[test]
    fn extract_author_email_empty_input() {
        let email = extract_author_email_from_commit(b"");
        assert!(email.is_none());
    }

    #[test]
    fn extract_author_email_complex_name() {
        let raw =
            b"tree abc123\nauthor John Q. Public Jr. <john.public@company.org> 1700000000 +0000\n";
        let email = extract_author_email_from_commit(raw);
        assert_eq!(email.as_deref(), Some("john.public@company.org"));
    }

    #[test]
    fn extract_author_email_from_gpg_signed_commit() {
        // GPG-signed commits have a gpgsig header between tree and author
        let raw = b"tree abc123\ngpgsig -----BEGIN PGP SIGNATURE-----\n\n iQIzBAEB...\n -----END PGP SIGNATURE-----\nauthor Alice <alice@signed.com> 1700000000 +0000\n\nmessage\n";
        let email = extract_author_email_from_commit(raw);
        assert_eq!(email.as_deref(), Some("alice@signed.com"));
    }

    // -- parse_branches --

    #[test]
    fn parse_branches_empty() {
        assert!(parse_branches("").is_empty());
    }

    #[test]
    fn parse_branches_single() {
        let output = "main\tabc1234\t2026-02-19T10:00:00+00:00\n";
        let branches = parse_branches(output);
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].name, "main");
        assert_eq!(branches[0].sha, "abc1234");
        assert_eq!(branches[0].updated_at, "2026-02-19T10:00:00+00:00");
    }

    #[test]
    fn parse_branches_missing_date() {
        let output = "main\tabc1234\n";
        let branches = parse_branches(output);
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].name, "main");
        assert_eq!(branches[0].updated_at, "");
    }

    // -- parse_log --

    #[test]
    fn parse_log_too_few_fields() {
        // Only 5 fields instead of 8 — should be skipped
        let output = "abc123\0msg\0alice\0alice@e.com\02026-01-01\n";
        let commits = parse_log(output);
        assert!(commits.is_empty());
    }

    #[test]
    fn parse_log_multiple_commits() {
        let line1 = "aaa\0msg1\0a\0a@e.com\02026-01-01\0c\0c@e.com\02026-01-01";
        let line2 = "bbb\0msg2\0b\0b@e.com\02026-01-02\0c\0c@e.com\02026-01-02";
        let output = format!("{line1}\n{line2}\n");
        let commits = parse_log(&output);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "aaa");
        assert_eq!(commits[1].sha, "bbb");
    }

    // -- parse_ls_tree --

    #[test]
    fn parse_ls_tree_malformed_line() {
        // Missing tab separator
        let output = "100644 blob abc1234 1234 README.md\n";
        let entries = parse_ls_tree(output);
        assert!(entries.is_empty(), "line without tab should be skipped");
    }

    #[test]
    fn parse_ls_tree_too_few_meta_parts() {
        // Only 2 meta parts instead of 4
        let output = "100644 blob\tREADME.md\n";
        let entries = parse_ls_tree(output);
        assert!(entries.is_empty());
    }

    // -- validate_git_ref --

    #[test]
    fn validate_ref_rejects_backtick() {
        assert!(validate_git_ref("`cmd`").is_err());
    }

    #[test]
    fn validate_ref_rejects_newline() {
        assert!(validate_git_ref("foo\nbar").is_err());
    }

    #[test]
    fn validate_ref_rejects_null_byte() {
        assert!(validate_git_ref("foo\0bar").is_err());
    }

    #[test]
    fn validate_ref_rejects_space() {
        assert!(validate_git_ref("foo bar").is_err());
    }

    // -- validate_path --

    #[test]
    fn validate_path_rejects_null() {
        assert!(validate_path("src/\0main.rs").is_err());
    }
}
