use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::response::Response;
use axum::routing::{get, post};
use http_body_util::BodyExt;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::{password, token};
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Authenticated git user (from HTTP Basic Auth).
pub struct GitUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub ip_addr: Option<String>,
}

/// Resolved project from /:owner/:repo path.
pub struct ResolvedProject {
    pub project_id: Uuid,
    pub repo_disk_path: PathBuf,
    pub default_branch: String,
    pub visibility: String,
}

#[derive(Debug, Deserialize)]
struct InfoRefsQuery {
    service: Option<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{owner}/{repo}/info/refs", get(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(receive_pack))
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

/// Validate a git ref name to prevent injection.
#[allow(dead_code)] // used in tests; available for future handler validation
fn validate_ref(git_ref: &str) -> Result<(), ApiError> {
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

// ---------------------------------------------------------------------------
// Basic Auth
// ---------------------------------------------------------------------------

/// Authenticate a git client via HTTP Basic Auth.
///
/// Tries the password as an API token first, then falls back to password verification.
/// This is NOT an axum extractor — called explicitly by smart HTTP handlers.
pub async fn authenticate_basic(headers: &HeaderMap, pool: &PgPool) -> Result<GitUser, ApiError> {
    let (username, password_raw) = extract_basic_credentials(headers)?;

    // Look up user by name
    let user = sqlx::query!(
        r#"
        SELECT id, name, password_hash, is_active
        FROM users WHERE name = $1
        "#,
        username,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::Unauthorized)?;

    if !user.is_active {
        return Err(ApiError::Unauthorized);
    }

    // Try password as API token first
    let token_hash = token::hash_token(&password_raw);
    let token_match = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!: i64"
        FROM api_tokens
        WHERE token_hash = $1
          AND user_id = $2
          AND (expires_at IS NULL OR expires_at > now())
        "#,
        token_hash,
        user.id,
    )
    .fetch_one(pool)
    .await?;

    if token_match > 0 {
        return Ok(GitUser {
            user_id: user.id,
            user_name: user.name,
            ip_addr: None,
        });
    }

    // Fall back to password verification
    let valid = password::verify_password(&password_raw, &user.password_hash)
        .map_err(ApiError::Internal)?;
    if !valid {
        return Err(ApiError::Unauthorized);
    }

    Ok(GitUser {
        user_id: user.id,
        user_name: user.name,
        ip_addr: None,
    })
}

/// Extract username and password from HTTP Basic Auth header.
fn extract_basic_credentials(headers: &HeaderMap) -> Result<(String, String), ApiError> {
    let auth_value = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized)?;

    let encoded = auth_value
        .strip_prefix("Basic ")
        .ok_or(ApiError::Unauthorized)?;

    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .map_err(|_| ApiError::Unauthorized)?;

    let decoded_str = String::from_utf8(decoded).map_err(|_| ApiError::Unauthorized)?;

    let (username, password_raw) = decoded_str.split_once(':').ok_or(ApiError::Unauthorized)?;

    if username.is_empty() {
        return Err(ApiError::Unauthorized);
    }

    Ok((username.to_owned(), password_raw.to_owned()))
}

// ---------------------------------------------------------------------------
// Project resolution
// ---------------------------------------------------------------------------

/// Resolve an /:owner/:repo path to a project in the database.
pub async fn resolve_project(
    pool: &PgPool,
    config: &crate::config::Config,
    owner: &str,
    repo: &str,
) -> Result<ResolvedProject, ApiError> {
    // Strip .git suffix if present
    let repo_name = repo.strip_suffix(".git").unwrap_or(repo);

    let row = sqlx::query!(
        r#"
        SELECT p.id, p.repo_path, p.default_branch, p.visibility
        FROM projects p
        JOIN users u ON u.id = p.owner_id
        WHERE u.name = $1 AND p.name = $2 AND p.is_active = true
        "#,
        owner,
        repo_name,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("repository".into()))?;

    let repo_disk_path = row.repo_path.map_or_else(
        || {
            config
                .git_repos_path
                .join(owner)
                .join(format!("{repo_name}.git"))
        },
        PathBuf::from,
    );

    Ok(ResolvedProject {
        project_id: row.id,
        repo_disk_path,
        default_branch: row.default_branch,
        visibility: row.visibility,
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /:owner/:repo/info/refs?service=git-upload-pack|git-receive-pack`
///
/// Returns ref advertisement with pkt-line header.
#[tracing::instrument(skip(state), fields(%owner, %repo), err)]
async fn info_refs(
    State(state): State<AppState>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    Query(query): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let service = query
        .service
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("service query parameter required".into()))?;

    if service != "git-upload-pack" && service != "git-receive-pack" {
        return Err(ApiError::BadRequest("invalid service".into()));
    }

    let project = resolve_project(&state.pool, &state.config, &owner, &repo).await?;

    // Auth + RBAC
    check_access(&state, &headers, &project, service == "git-upload-pack").await?;

    // Run git service with --advertise-refs
    let output = tokio::process::Command::new("git")
        .arg(service)
        .arg("--stateless-rpc")
        .arg("--advertise-refs")
        .arg(&project.repo_disk_path)
        .output()
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to spawn git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::error!(stderr = %stderr, "git info/refs failed");
        return Err(ApiError::Internal(anyhow::anyhow!("git failed: {stderr}")));
    }

    // Build pkt-line header + git output
    let mut body = pkt_line_header(service);
    body.extend_from_slice(&output.stdout);

    let content_type = format!("application/x-{service}-advertisement");
    Ok(Response::builder()
        .header("Content-Type", content_type)
        .header("Cache-Control", "no-cache")
        .body(Body::from(body))
        .expect("response builder"))
}

/// `POST /:owner/:repo/git-upload-pack`
///
/// Clone/fetch: streams request body to git process stdin, streams stdout back.
#[tracing::instrument(skip(state, request), fields(%owner, %repo), err)]
async fn upload_pack(
    State(state): State<AppState>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let project = resolve_project(&state.pool, &state.config, &owner, &repo).await?;

    check_access(&state, request.headers(), &project, true).await?;

    run_git_service(&project.repo_disk_path, "upload-pack", request.into_body())
}

/// `POST /:owner/:repo/git-receive-pack`
///
/// Push: collects full body/stdout (waits for completion to run hooks).
#[tracing::instrument(skip(state, request), fields(%owner, %repo), err)]
async fn receive_pack(
    State(state): State<AppState>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let project = resolve_project(&state.pool, &state.config, &owner, &repo).await?;

    let git_user = check_access(&state, request.headers(), &project, false)
        .await?
        .expect("receive-pack always authenticates");

    let body = request.into_body();

    // Spawn git receive-pack
    let mut child = tokio::process::Command::new("git")
        .arg("receive-pack")
        .arg("--stateless-rpc")
        .arg(&project.repo_disk_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to spawn git: {e}")))?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");

    // Pipe body to stdin and read stdout concurrently
    let (stdin_result, stdout_bytes) = tokio::join!(
        async {
            let bytes = body
                .collect()
                .await
                .map_err(|e| anyhow::anyhow!("body read failed: {e}"))?
                .to_bytes();
            stdin.write_all(&bytes).await?;
            stdin.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        },
        async {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        }
    );

    stdin_result.map_err(ApiError::Internal)?;
    let output =
        stdout_bytes.map_err(|e| ApiError::Internal(anyhow::anyhow!("stdout read: {e}")))?;

    let status = child
        .wait()
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("git wait: {e}")))?;

    if status.success() {
        // Run post-receive hooks in background
        let hook_state = state.clone();
        let params = super::hooks::PostReceiveParams {
            project_id: project.project_id,
            user_id: git_user.user_id,
            user_name: git_user.user_name.clone(),
            repo_path: project.repo_disk_path.clone(),
            default_branch: project.default_branch.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = super::hooks::post_receive(&hook_state, &params).await {
                tracing::error!(error = %e, "post-receive hook failed");
            }
        });
    }

    // Audit log
    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: git_user.user_id,
            actor_name: &git_user.user_name,
            action: "git.push",
            resource: "project",
            resource_id: Some(project.project_id),
            project_id: Some(project.project_id),
            detail: None,
            ip_addr: git_user.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Response::builder()
        .header("Content-Type", "application/x-git-receive-pack-result")
        .header("Cache-Control", "no-cache")
        .body(Body::from(output))
        .expect("response builder"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check access for a git operation. Returns the authenticated user (if any).
///
/// For read operations on public repos, returns `Ok(None)` (no auth needed).
/// For all other cases, authenticates and checks RBAC.
async fn check_access(
    state: &AppState,
    headers: &HeaderMap,
    project: &ResolvedProject,
    is_read: bool,
) -> Result<Option<GitUser>, ApiError> {
    if is_read && project.visibility == "public" {
        return Ok(None);
    }

    // Authenticate
    let git_user = authenticate_basic(headers, &state.pool).await?;

    if is_read && project.visibility == "internal" {
        // Any authenticated user can read internal projects
        return Ok(Some(git_user));
    }

    // Check project-scoped permission
    let perm = if is_read {
        Permission::ProjectRead
    } else {
        Permission::ProjectWrite
    };

    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        git_user.user_id,
        Some(project.project_id),
        perm,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

    Ok(Some(git_user))
}

/// Build pkt-line header for info/refs response.
fn pkt_line_header(service: &str) -> Vec<u8> {
    let announcement = format!("# service={service}\n");
    let pkt_len = announcement.len() + 4; // 4 bytes for the length prefix itself
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("{pkt_len:04x}").as_bytes());
    buf.extend_from_slice(announcement.as_bytes());
    buf.extend_from_slice(b"0000"); // flush-pkt
    buf
}

/// Run a git service (upload-pack or receive-pack) with bidirectional streaming.
///
/// Used for upload-pack where we can stream the response progressively.
fn run_git_service(repo_path: &Path, service: &str, body: Body) -> Result<Response, ApiError> {
    let mut child = tokio::process::Command::new("git")
        .arg(service)
        .arg("--stateless-rpc")
        .arg(repo_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("failed to spawn git: {e}")))?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");

    // Pipe request body to stdin in background
    tokio::spawn(async move {
        let result = async {
            let bytes = body
                .collect()
                .await
                .map_err(|e| anyhow::anyhow!("body read: {e}"))?
                .to_bytes();
            stdin.write_all(&bytes).await?;
            stdin.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "stdin pipe failed");
        }
    });

    // Stream stdout as response body
    let stream = ReaderStream::new(stdout);
    let response_body = Body::from_stream(stream);

    let content_type = format!("application/x-git-{service}-result");
    Ok(Response::builder()
        .header("Content-Type", content_type)
        .header("Cache-Control", "no-cache")
        .body(response_body)
        .expect("response builder"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkt_line_header_upload_pack() {
        let header = pkt_line_header("git-upload-pack");
        let s = String::from_utf8(header).unwrap();
        // "# service=git-upload-pack\n" = 26 chars + 4 hex prefix = 30
        assert!(s.starts_with("001e"));
        assert!(s.contains("# service=git-upload-pack\n"));
        assert!(s.ends_with("0000"));
    }

    #[test]
    fn pkt_line_header_receive_pack() {
        let header = pkt_line_header("git-receive-pack");
        let s = String::from_utf8(header).unwrap();
        assert!(s.contains("# service=git-receive-pack\n"));
        assert!(s.ends_with("0000"));
    }

    #[test]
    fn validate_ref_rejects_dangerous_input() {
        assert!(validate_ref("main").is_ok());
        assert!(validate_ref("feature/foo").is_ok());
        assert!(validate_ref("v1.0.0").is_ok());

        assert!(validate_ref("").is_err());
        assert!(validate_ref("foo..bar").is_err());
        assert!(validate_ref("foo;rm -rf").is_err());
        assert!(validate_ref("foo|bar").is_err());
        assert!(validate_ref("$HOME").is_err());
        assert!(validate_ref("foo`cmd`").is_err());
        assert!(validate_ref("foo\nbar").is_err());
        assert!(validate_ref("foo\0bar").is_err());
        assert!(validate_ref("foo bar").is_err());
    }

    #[test]
    fn extract_basic_credentials_valid() {
        let mut headers = HeaderMap::new();
        // base64("alice:secret123") = "YWxpY2U6c2VjcmV0MTIz"
        headers.insert(AUTHORIZATION, "Basic YWxpY2U6c2VjcmV0MTIz".parse().unwrap());
        let (user, pass) = extract_basic_credentials(&headers).unwrap();
        assert_eq!(user, "alice");
        assert_eq!(pass, "secret123");
    }

    #[test]
    fn extract_basic_credentials_missing_header() {
        let headers = HeaderMap::new();
        assert!(extract_basic_credentials(&headers).is_err());
    }

    #[test]
    fn extract_basic_credentials_not_basic() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer token123".parse().unwrap());
        assert!(extract_basic_credentials(&headers).is_err());
    }

    #[test]
    fn extract_basic_credentials_password_with_colon() {
        let mut headers = HeaderMap::new();
        // base64("alice:pass:word") = "YWxpY2U6cGFzczp3b3Jk"
        headers.insert(AUTHORIZATION, "Basic YWxpY2U6cGFzczp3b3Jk".parse().unwrap());
        let (user, pass) = extract_basic_credentials(&headers).unwrap();
        assert_eq!(user, "alice");
        assert_eq!(pass, "pass:word");
    }
}
