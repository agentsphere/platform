use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::audit::{AuditEntry, send_audit};
use crate::auth::{password, token};
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Authenticated git user (from HTTP Basic Auth).
#[derive(Clone)]
pub struct GitUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub ip_addr: Option<String>,
    /// Hard project boundary from API token (if token-authenticated).
    pub boundary_project_id: Option<Uuid>,
    /// Hard workspace boundary from API token (if token-authenticated).
    pub boundary_workspace_id: Option<Uuid>,
    /// Token permission scopes (None = password/SSH auth, Some = API token auth).
    pub token_scopes: Option<Vec<String>>,
}

/// Resolved project from /:owner/:repo path.
#[derive(Clone)]
pub struct ResolvedProject {
    pub project_id: Uuid,
    pub owner_id: Uuid,
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
        .layer(axum::middleware::map_response(add_www_authenticate))
}

/// Add `WWW-Authenticate: Basic` to 401 responses on git routes.
/// Placed only on the git smart HTTP router so the browser SPA doesn't
/// get a native credentials dialog for API 401s.
async fn add_www_authenticate(response: Response) -> Response {
    if response.status() == axum::http::StatusCode::UNAUTHORIZED {
        let (mut parts, body) = response.into_parts();
        parts.headers.insert(
            axum::http::header::WWW_AUTHENTICATE,
            "Basic realm=\"platform\""
                .parse()
                .expect("valid header value"),
        );
        Response::from_parts(parts, body)
    } else {
        response
    }
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
    let user_row = sqlx::query!(
        r#"
        SELECT id, name, password_hash, is_active
        FROM users WHERE name = $1
        "#,
        username,
    )
    .fetch_optional(pool)
    .await?;

    // Try password as API token first (SHA-256 is constant-time relative to user existence)
    let token_hash = token::hash_token(&password_raw);
    let token_row = if let Some(ref user) = user_row {
        sqlx::query!(
            r#"
        SELECT project_id, scope_workspace_id, scopes
        FROM api_tokens
        WHERE token_hash = $1
          AND user_id = $2
          AND (expires_at IS NULL OR expires_at > now())
        "#,
            token_hash,
            user.id,
        )
        .fetch_optional(pool)
        .await?
    } else {
        None
    };

    if let Some(token_row) = token_row {
        // A25: Replace .expect() with proper error handling
        let user = user_row.ok_or(ApiError::Unauthorized)?;
        if !user.is_active {
            return Err(ApiError::Unauthorized);
        }
        return Ok(GitUser {
            user_id: user.id,
            user_name: user.name,
            ip_addr: None,
            boundary_project_id: token_row.project_id,
            boundary_workspace_id: token_row.scope_workspace_id,
            token_scopes: Some(token_row.scopes), // A8: enforce token scopes
        });
    }

    // Fallback: token-only auth (supports GIT_ASKPASS where the token is used
    // as both username and password, so username won't match any user name).
    if user_row.is_none() {
        let token_with_user = sqlx::query!(
            r#"
            SELECT t.project_id, t.scope_workspace_id, t.scopes,
                   u.id as "user_id!: Uuid", u.name as "user_name!: String",
                   u.is_active as "is_active!: bool"
            FROM api_tokens t
            JOIN users u ON u.id = t.user_id
            WHERE t.token_hash = $1
              AND (t.expires_at IS NULL OR t.expires_at > now())
            "#,
            token_hash,
        )
        .fetch_optional(pool)
        .await?;

        if let Some(row) = token_with_user {
            if !row.is_active {
                return Err(ApiError::Unauthorized);
            }
            tracing::debug!(user_id = %row.user_id, "git auth via token-only fallback (GIT_ASKPASS)");
            return Ok(GitUser {
                user_id: row.user_id,
                user_name: row.user_name,
                ip_addr: None,
                boundary_project_id: row.project_id,
                boundary_workspace_id: row.scope_workspace_id,
                token_scopes: Some(row.scopes), // A8: enforce token scopes
            });
        }
    }

    // Always run argon2 verify to prevent timing oracle (user enumeration)
    let hash_to_verify = user_row
        .as_ref()
        .map_or_else(|| password::dummy_hash(), |u| u.password_hash.as_str());

    let valid = password::verify_password(&password_raw, hash_to_verify);

    let Some(user) = user_row else {
        return Err(ApiError::Unauthorized);
    };
    if !user.is_active || !valid {
        return Err(ApiError::Unauthorized);
    }

    Ok(GitUser {
        user_id: user.id,
        user_name: user.name,
        ip_addr: None,
        boundary_project_id: None,
        boundary_workspace_id: None,
        token_scopes: None, // Password auth — no token scopes
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
        SELECT p.id, p.owner_id, p.repo_path, p.default_branch, p.visibility
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
        owner_id: row.owner_id,
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
#[tracing::instrument(skip(state, headers), fields(%owner, %repo))]
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

    // Auth + RBAC — first git request often has no credentials (WWW-Authenticate flow).
    // Log at debug for 401 (expected), error for real failures.
    if let Err(e) = check_access(&state, &headers, &project, service == "git-upload-pack").await {
        match &e {
            ApiError::Unauthorized => {
                tracing::debug!(%owner, %repo, "git info/refs: no credentials (client will retry via WWW-Authenticate)");
            }
            _ => {
                tracing::error!(error = %e, %owner, %repo, "git info/refs auth failed");
            }
        }
        return Err(e);
    }

    // Run git service with --advertise-refs
    // The service query value is "git-upload-pack" or "git-receive-pack" but
    // the git subcommand names are "upload-pack" and "receive-pack".
    let git_cmd = service.strip_prefix("git-").unwrap_or(service);
    let output = tokio::process::Command::new("git")
        .arg(git_cmd)
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

/// Check branch protection rules for all ref updates in a push.
pub async fn enforce_push_protection(
    state: &AppState,
    project: &ResolvedProject,
    git_user: &GitUser,
    ref_updates: &[super::hooks::RefUpdate],
) -> Result<(), ApiError> {
    for update in ref_updates {
        let Some(branch) = update.refname.strip_prefix("refs/heads/") else {
            continue;
        };
        let rule = super::protection::get_protection(&state.pool, project.project_id, branch)
            .await
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("protection check: {e}")))?;

        let Some(rule) = rule else { continue };

        let is_admin = if rule.allow_admin_bypass {
            resolver::has_permission(
                &state.pool,
                &state.valkey,
                git_user.user_id,
                Some(project.project_id),
                Permission::AdminUsers,
            )
            .await
            .unwrap_or(false)
                || project.owner_id == git_user.user_id
        } else {
            false
        };

        if is_admin {
            continue;
        }

        if rule.require_pr {
            return Err(ApiError::Forbidden);
        }

        if rule.block_force_push
            && super::protection::is_force_push(
                &project.repo_disk_path,
                &update.old_sha,
                &update.new_sha,
            )
            .await
        {
            return Err(ApiError::Forbidden);
        }
    }
    Ok(())
}

/// `POST /:owner/:repo/git-receive-pack`
///
/// Push: streams body to git stdin (only pkt-line header buffered for protection checks).
#[tracing::instrument(skip(state, request), fields(%owner, %repo), err)]
#[allow(clippy::too_many_lines)]
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

    // A19: Stream body to git stdin instead of buffering the entire pack in memory.
    // Phase 1: Buffer only the pkt-line header (ref commands, typically <1 KB)
    //          until the 0000 flush-pkt, then parse and enforce branch protection.
    // Phase 2: Pipe the buffered header + remaining body frames to git stdin.

    let mut pkt_buf = Vec::new();
    let mut body_stream = body.into_data_stream();
    let mut remaining_frame: Option<bytes::Bytes> = None;

    // Read frames until we find the flush-pkt (0000) that ends the ref command section
    loop {
        let frame = match body_stream.next().await {
            Some(Ok(frame)) => frame,
            Some(Err(e)) => return Err(ApiError::Internal(anyhow::anyhow!("body read: {e}"))),
            None => return Err(ApiError::BadRequest("incomplete pack data".into())),
        };
        pkt_buf.extend_from_slice(&frame);

        // Check for flush-pkt using the same logic as the SSH path
        if let Some(flush_pos) = super::ssh_server::find_flush_pkt(&pkt_buf) {
            // Everything after the flush-pkt is PACK data — don't buffer it
            if flush_pos < pkt_buf.len() {
                remaining_frame = Some(bytes::Bytes::copy_from_slice(&pkt_buf[flush_pos..]));
                pkt_buf.truncate(flush_pos);
            }
            break;
        }

        // Safety: cap buffer at 1 MB (same as SSH path) to prevent abuse
        if pkt_buf.len() > 1_048_576 {
            return Err(ApiError::BadRequest("pack header too large".into()));
        }
    }

    // Parse pushed refs from the pkt-line header
    let ref_updates = super::hooks::parse_pack_commands(&pkt_buf);
    let pushed_branches = super::hooks::extract_pushed_branches(&ref_updates);

    // Check branch protection rules before piping anything to git
    enforce_push_protection(&state, &project, &git_user, &ref_updates).await?;

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

    // Phase 2: Pipe buffered pkt-line header + stream remaining body to git stdin
    let (stdin_result, stdout_bytes) = tokio::join!(
        async {
            // Write the buffered pkt-line header
            stdin.write_all(&pkt_buf).await?;
            // Write any PACK data that arrived in the same frame as the flush-pkt
            if let Some(remaining) = remaining_frame {
                stdin.write_all(&remaining).await?;
            }
            // Stream remaining body frames directly to stdin (no buffering)
            while let Some(frame_result) = body_stream.next().await {
                let frame = frame_result.map_err(std::io::Error::other)?;
                stdin.write_all(&frame).await?;
            }
            stdin.shutdown().await?;
            Ok::<(), std::io::Error>(())
        },
        async {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        }
    );

    stdin_result.map_err(|e| ApiError::Internal(anyhow::anyhow!("stdin write: {e}")))?;
    let output =
        stdout_bytes.map_err(|e| ApiError::Internal(anyhow::anyhow!("stdout read: {e}")))?;

    let status = child
        .wait()
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("git wait: {e}")))?;

    if status.success() {
        tracing::info!(
            %owner, %repo,
            branches = ?pushed_branches,
            "receive-pack succeeded, dispatching post-receive"
        );
        // Run post-receive hooks in background
        let hook_state = state.clone();
        let pushed_tags = super::hooks::extract_pushed_tags(&ref_updates);
        let params = super::hooks::PostReceiveParams {
            project_id: project.project_id,
            user_id: git_user.user_id,
            user_name: git_user.user_name.clone(),
            repo_path: project.repo_disk_path.clone(),
            default_branch: project.default_branch.clone(),
            pushed_branches,
            pushed_tags,
        };
        tokio::spawn(async move {
            if let Err(e) = super::hooks::post_receive(&hook_state, &params).await {
                tracing::error!(error = %e, "post-receive hook failed");
            }
        });
    }

    // Audit log
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: git_user.user_id,
            actor_name: git_user.user_name.clone(),
            action: "git.push".into(),
            resource: "project".into(),
            resource_id: Some(project.project_id),
            project_id: Some(project.project_id),
            detail: None,
            ip_addr: git_user.ip_addr.clone(),
        },
    );

    Ok(Response::builder()
        .header("Content-Type", "application/x-git-receive-pack-result")
        .header("Cache-Control", "no-cache")
        .body(Body::from(output))
        .expect("response builder"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check RBAC access for an already-authenticated git user.
///
/// Enforces token scope (project + workspace), visibility rules, and permission checks.
/// Returns `Ok(())` if allowed, `Err(NotFound)` if denied (to avoid leaking repo existence).
pub async fn check_access_for_user(
    state: &AppState,
    git_user: &GitUser,
    project: &ResolvedProject,
    is_read: bool,
) -> Result<(), ApiError> {
    // Enforce hard project scope from API token
    if let Some(scope_pid) = git_user.boundary_project_id
        && scope_pid != project.project_id
    {
        return Err(ApiError::NotFound("repository".into()));
    }

    // Enforce hard workspace scope from API token
    if let Some(scope_wid) = git_user.boundary_workspace_id {
        let in_workspace = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM projects WHERE id = $1 AND workspace_id = $2 AND is_active = true) as "exists!: bool""#,
            project.project_id, scope_wid,
        )
        .fetch_one(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("{e}")))?;
        if !in_workspace {
            return Err(ApiError::NotFound("repository".into()));
        }
    }

    // Public or internal repos: any authenticated user can read
    if is_read && (project.visibility == "public" || project.visibility == "internal") {
        return Ok(());
    }

    // Check project-scoped permission
    let perm = if is_read {
        Permission::ProjectRead
    } else {
        Permission::ProjectWrite
    };

    // A8: Use has_permission_scoped to enforce API token scopes
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        git_user.user_id,
        Some(project.project_id),
        perm,
        git_user.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::NotFound("repository".into()));
    }

    Ok(())
}

/// Check access for an HTTP git operation. Returns the authenticated user (if any).
///
/// For read operations on public repos, returns `Ok(None)` (no auth needed).
/// For all other cases, authenticates and delegates to `check_access_for_user`.
async fn check_access(
    state: &AppState,
    headers: &HeaderMap,
    project: &ResolvedProject,
    is_read: bool,
) -> Result<Option<GitUser>, ApiError> {
    // Public repos: allow unauthenticated reads
    if is_read && project.visibility == "public" {
        return Ok(None);
    }

    let git_user = authenticate_basic(headers, &state.pool).await?;
    // S52: rate-limit git basic auth — high enough for concurrent pipeline
    // clones (3 parallel steps × 2 calls each × multiple pipelines).
    crate::auth::rate_limit::check_rate(&state.valkey, "git_auth", &git_user.user_name, 200, 300)
        .await?;
    check_access_for_user(state, &git_user, project, is_read).await?;
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

    #[test]
    fn extract_basic_credentials_empty_username_rejected() {
        let mut headers = HeaderMap::new();
        // base64(":password") = "OnBhc3N3b3Jk"
        headers.insert(AUTHORIZATION, "Basic OnBhc3N3b3Jk".parse().unwrap());
        assert!(
            extract_basic_credentials(&headers).is_err(),
            "empty username should be rejected"
        );
    }

    #[test]
    fn extract_basic_credentials_empty_password_accepted() {
        let mut headers = HeaderMap::new();
        // base64("alice:") = "YWxpY2U6"
        headers.insert(AUTHORIZATION, "Basic YWxpY2U6".parse().unwrap());
        let (user, pass) = extract_basic_credentials(&headers).unwrap();
        assert_eq!(user, "alice");
        assert_eq!(pass, "");
    }

    #[test]
    fn extract_basic_credentials_no_colon_rejected() {
        let mut headers = HeaderMap::new();
        // base64("justausername") = "anVzdGF1c2VybmFtZQ=="
        headers.insert(AUTHORIZATION, "Basic anVzdGF1c2VybmFtZQ==".parse().unwrap());
        assert!(
            extract_basic_credentials(&headers).is_err(),
            "credentials without colon should be rejected"
        );
    }

    #[test]
    fn extract_basic_credentials_invalid_base64() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Basic !!!invalid!!!".parse().unwrap());
        assert!(extract_basic_credentials(&headers).is_err());
    }

    #[test]
    fn validate_ref_accepts_complex_names() {
        assert!(validate_ref("refs/heads/feature/my-branch").is_ok());
        assert!(validate_ref("refs/tags/v1.0.0-rc.1").is_ok());
        assert!(validate_ref("HEAD").is_ok());
        assert!(validate_ref("abc123def456").is_ok());
    }

    #[test]
    fn validate_ref_rejects_all_dangerous_chars() {
        let dangerous = &["..", ";", "|", "$", "`", "\n", "\0", " "];
        for ch in dangerous {
            let test_ref = format!("foo{ch}bar");
            assert!(
                validate_ref(&test_ref).is_err(),
                "should reject ref containing {ch:?}"
            );
        }
    }
}
