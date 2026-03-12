use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use ssh_key::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::audit::{AuditEntry, write_audit};
use crate::store::AppState;

use super::hooks;
use super::smart_http::{GitUser, check_access_for_user, resolve_project};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid command")]
    InvalidCommand,
    #[error("dangerous path rejected")]
    PathTraversal,
    #[error("unsupported service: {0}")]
    UnsupportedService(String),
}

// ---------------------------------------------------------------------------
// Command parsing
// ---------------------------------------------------------------------------

/// Parsed SSH git command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub owner: String,
    pub repo: String,
    pub is_read: bool,
}

/// Parse an SSH exec command like `git-upload-pack 'owner/repo.git'`.
///
/// Returns the owner, repo name (without `.git` suffix), and whether this is a
/// read operation (`git-upload-pack`) vs write (`git-receive-pack`).
pub fn parse_ssh_command(command: &str) -> Result<ParsedCommand, SshError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(SshError::InvalidCommand);
    }

    let (service, path) = command.split_once(' ').ok_or(SshError::InvalidCommand)?;

    let is_read = match service {
        "git-upload-pack" => true,
        "git-receive-pack" => false,
        _ => return Err(SshError::UnsupportedService(service.to_string())),
    };

    // Strip surrounding quotes (single or double)
    let path = path.trim();
    let path = strip_quotes(path);

    // Strip leading /
    let path = path.strip_prefix('/').unwrap_or(path);

    // Reject dangerous characters
    if path.contains("..")
        || path.contains('\0')
        || path.contains('\n')
        || path.contains(';')
        || path.contains('|')
        || path.contains('`')
        || path.contains('$')
        || path.contains(' ')
    {
        return Err(SshError::PathTraversal);
    }

    // Strip .git suffix
    let path = path.strip_suffix(".git").unwrap_or(path);

    // Split into owner/repo — must be exactly two segments
    let (owner, repo) = path.split_once('/').ok_or(SshError::InvalidCommand)?;

    if repo.contains('/') || owner.is_empty() || repo.is_empty() {
        return Err(SshError::InvalidCommand);
    }

    Ok(ParsedCommand {
        owner: owner.to_string(),
        repo: repo.to_string(),
        is_read,
    })
}

fn strip_quotes(s: &str) -> &str {
    if (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// SSH session handler
// ---------------------------------------------------------------------------

struct SshSessionHandler {
    state: AppState,
    git_user: Option<GitUser>,
    git_stdin: HashMap<ChannelId, tokio::process::ChildStdin>,
}

#[async_trait::async_trait]
impl russh::server::Handler for SshSessionHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        let fingerprint = public_key.fingerprint(ssh_key::HashAlg::Sha256).to_string();

        let row = sqlx::query!(
            r#"
            SELECT k.user_id, u.name as user_name
            FROM user_ssh_keys k
            JOIN users u ON u.id = k.user_id AND u.is_active = true
            WHERE k.fingerprint = $1
            "#,
            fingerprint,
        )
        .fetch_optional(&self.state.pool)
        .await?;

        if let Some(row) = row {
            // Update last_used_at (fire-and-forget)
            let pool = self.state.pool.clone();
            let fp = fingerprint.clone();
            tokio::spawn(async move {
                let _ = sqlx::query!(
                    "UPDATE user_ssh_keys SET last_used_at = now() WHERE fingerprint = $1",
                    fp,
                )
                .execute(&pool)
                .await;
            });

            self.git_user = Some(GitUser {
                user_id: row.user_id,
                user_name: row.user_name,
                ip_addr: None,
                boundary_project_id: None,
                boundary_workspace_id: None,
            });

            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command_str = String::from_utf8_lossy(data);
        let parsed = match parse_ssh_command(&command_str) {
            Ok(p) => p,
            Err(e) => {
                let truncated: &str = if command_str.len() > 256 {
                    &command_str[..256]
                } else {
                    &command_str
                };
                tracing::warn!(error = %e, command = %truncated, "SSH command rejected");
                send_exit_and_close(session.handle(), channel_id, 1);
                return Ok(());
            }
        };

        let Some(git_user) = &self.git_user else {
            send_exit_and_close(session.handle(), channel_id, 1);
            return Ok(());
        };

        let Ok(project) = resolve_project(
            &self.state.pool,
            &self.state.config,
            &parsed.owner,
            &parsed.repo,
        )
        .await
        else {
            send_exit_and_close(session.handle(), channel_id, 1);
            return Ok(());
        };

        if check_access_for_user(&self.state, git_user, &project, parsed.is_read)
            .await
            .is_err()
        {
            send_exit_and_close(session.handle(), channel_id, 1);
            return Ok(());
        }

        // Spawn git process (stateful, no --stateless-rpc for SSH)
        let service = if parsed.is_read {
            "upload-pack"
        } else {
            "receive-pack"
        };

        let mut child = match spawn_git(service, &project.repo_disk_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn git for SSH");
                send_exit_and_close(session.handle(), channel_id, 1);
                return Ok(());
            }
        };

        // Store stdin for data callback
        if let Some(stdin) = child.stdin.take() {
            self.git_stdin.insert(channel_id, stdin);
        }

        let mut stdout = child.stdout.take().expect("stdout piped");
        let handle = session.handle();
        let state = self.state.clone();
        let user_id = git_user.user_id;
        let user_name = git_user.user_name.clone();
        let is_push = !parsed.is_read;

        // Pipe git stdout → SSH channel, then handle cleanup
        tokio::spawn(Box::pin(async move {
            pipe_git_to_ssh(&mut stdout, &handle, channel_id).await;

            let exit_code = match child.wait().await {
                Ok(status) => status.code().map_or(1, i32::unsigned_abs),
                Err(_) => 1,
            };

            if is_push && exit_code == 0 {
                handle_post_push(&state, user_id, &user_name, &project).await;
            }

            let _ = handle.exit_status_request(channel_id, exit_code).await;
            let _ = handle.eof(channel_id).await;
            let _ = handle.close(channel_id).await;
        }));

        Ok(())
    }

    async fn data(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(stdin) = self.git_stdin.get_mut(&channel_id)
            && stdin.write_all(data).await.is_err()
        {
            self.git_stdin.remove(&channel_id);
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Client signaled EOF — close git stdin
        self.git_stdin.remove(&channel_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers (extracted for < 100 lines per function)
// ---------------------------------------------------------------------------

/// Send exit status, EOF, and close on a channel (fire-and-forget).
fn send_exit_and_close(handle: russh::server::Handle, channel_id: ChannelId, code: u32) {
    tokio::spawn(async move {
        let _ = handle.exit_status_request(channel_id, code).await;
        let _ = handle.eof(channel_id).await;
        let _ = handle.close(channel_id).await;
    });
}

/// Spawn a git subprocess for SSH (stateful, no `--stateless-rpc`).
fn spawn_git(
    service: &str,
    repo_path: &std::path::Path,
) -> Result<tokio::process::Child, std::io::Error> {
    tokio::process::Command::new("git")
        .arg(service)
        .arg(repo_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
}

/// Pipe git subprocess stdout to SSH channel.
async fn pipe_git_to_ssh(
    stdout: &mut tokio::process::ChildStdout,
    handle: &russh::server::Handle,
    channel_id: ChannelId,
) {
    let mut buf = vec![0u8; 32768];
    loop {
        match stdout.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let data = russh::CryptoVec::from_slice(&buf[..n]);
                if handle.data(channel_id, data).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "git stdout read ended");
                break;
            }
        }
    }
}

/// Run post-push hooks and write audit log.
async fn handle_post_push(
    state: &AppState,
    user_id: uuid::Uuid,
    user_name: &str,
    project: &super::smart_http::ResolvedProject,
) {
    let params = hooks::PostReceiveParams {
        project_id: project.project_id,
        user_id,
        user_name: user_name.to_string(),
        repo_path: project.repo_disk_path.clone(),
        default_branch: project.default_branch.clone(),
        pushed_branches: Vec::new(), // SSH path: fall back to default_branch for now
        pushed_tags: Vec::new(),
    };
    if let Err(e) = hooks::post_receive(state, &params).await {
        tracing::error!(error = %e, "SSH post-receive hook failed");
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: user_id,
            actor_name: user_name,
            action: "git.push",
            resource: "project",
            resource_id: Some(project.project_id),
            project_id: Some(project.project_id),
            detail: Some(serde_json::json!({"protocol": "ssh"})),
            ip_addr: None,
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// Host key management
// ---------------------------------------------------------------------------

/// Load an existing ED25519 host key or generate a new one via `ssh-keygen`.
async fn load_or_generate_host_key(path: &str) -> Result<PrivateKey, anyhow::Error> {
    let key_path = Path::new(path);

    if !key_path.exists() {
        generate_host_key(key_path).await?;
    }

    // Warn about loose permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = tokio::fs::metadata(key_path).await?;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %key_path.display(),
                mode = format!("{mode:04o}"),
                "SSH host key has loose permissions (should be 0600)"
            );
        }
    }

    let key = russh_keys::load_secret_key(key_path, None)?;
    tracing::info!(path = %key_path.display(), "SSH host key loaded");
    Ok(key)
}

/// Generate an ED25519 host key using `ssh-keygen`.
async fn generate_host_key(key_path: &Path) -> Result<(), anyhow::Error> {
    tracing::info!(path = %key_path.display(), "generating SSH host key");

    if let Some(parent) = key_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let output = tokio::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(key_path)
        .args(["-N", "", "-q"])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("ssh-keygen failed: {stderr}"));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600)).await?;
    }

    tracing::info!(path = %key_path.display(), "SSH host key generated");
    Ok(())
}

// ---------------------------------------------------------------------------
// Server main loop
// ---------------------------------------------------------------------------

/// Run the SSH git server. Spawned as a background task from `main.rs`.
#[tracing::instrument(skip(state, shutdown), err)]
pub async fn run(
    state: AppState,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> Result<(), anyhow::Error> {
    let listen_addr = match &state.config.ssh_listen {
        Some(addr) => addr.clone(),
        None => return Ok(()),
    };

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    tracing::info!(addr = %listen_addr, "SSH server listening");

    run_with_listener(state, listener, &mut shutdown).await
}

/// Run the SSH server on a pre-bound listener. Returns when shutdown is signalled.
///
/// This is the core accept loop, factored out so E2E tests can bind to port 0
/// and discover the actual port before connecting.
pub async fn run_with_listener(
    state: AppState,
    listener: tokio::net::TcpListener,
    shutdown: &mut tokio::sync::watch::Receiver<()>,
) -> Result<(), anyhow::Error> {
    let key_pair = load_or_generate_host_key(&state.config.ssh_host_key_path).await?;

    let config = Arc::new(russh::server::Config {
        keys: vec![key_pair],
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        maximum_packet_size: 65536,
        ..Default::default()
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, peer_addr)) => {
                        let handler = SshSessionHandler {
                            state: state.clone(),
                            git_user: None,
                            git_stdin: HashMap::new(),
                        };
                        let cfg = config.clone();
                        tokio::spawn(async move {
                            if let Err(e) = russh::server::run_stream(cfg, stream, handler).await {
                                tracing::debug!(peer = %peer_addr, error = %e, "SSH session ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "SSH accept failed");
                    }
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("SSH server shutting down");
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_exec_command_upload_pack() {
        let result = parse_ssh_command("git-upload-pack 'owner/repo.git'").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
        assert!(result.is_read);
    }

    #[test]
    fn test_parse_exec_command_receive_pack() {
        let result = parse_ssh_command("git-receive-pack 'owner/repo.git'").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
        assert!(!result.is_read);
    }

    #[test]
    fn test_parse_exec_command_no_quotes() {
        let result = parse_ssh_command("git-upload-pack owner/repo.git").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
        assert!(result.is_read);
    }

    #[test]
    fn test_parse_exec_command_double_quotes() {
        let result = parse_ssh_command("git-upload-pack \"owner/repo.git\"").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
    }

    #[test]
    fn test_parse_exec_command_leading_slash() {
        let result = parse_ssh_command("git-upload-pack '/owner/repo.git'").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
    }

    #[test]
    fn test_parse_exec_command_invalid_service() {
        let result = parse_ssh_command("git-archive 'owner/repo'");
        assert!(
            matches!(result, Err(SshError::UnsupportedService(_))),
            "expected UnsupportedService, got: {result:?}"
        );
    }

    #[test]
    fn test_parse_exec_command_empty() {
        let result = parse_ssh_command("");
        assert!(
            matches!(result, Err(SshError::InvalidCommand)),
            "expected InvalidCommand, got: {result:?}"
        );
    }

    #[test]
    fn test_parse_exec_command_path_traversal() {
        let result = parse_ssh_command("git-upload-pack '../etc/passwd'");
        assert!(
            matches!(result, Err(SshError::PathTraversal)),
            "expected PathTraversal, got: {result:?}"
        );
    }

    #[test]
    fn test_parse_exec_command_absolute_path() {
        // /etc/passwd → after stripping leading /, becomes "etc/passwd"
        // which parses as owner="etc", repo="passwd" — valid parse.
        // Security comes from resolve_project() not finding the repo in DB.
        let result = parse_ssh_command("git-upload-pack '/etc/passwd'").unwrap();
        assert_eq!(result.owner, "etc");
        assert_eq!(result.repo, "passwd");
    }

    #[test]
    fn test_parse_exec_command_no_path() {
        let result = parse_ssh_command("git-upload-pack");
        assert!(
            matches!(result, Err(SshError::InvalidCommand)),
            "expected InvalidCommand, got: {result:?}"
        );
    }

    #[test]
    fn test_parse_exec_command_shell_injection() {
        let result = parse_ssh_command("git-upload-pack 'foo;rm -rf /'");
        assert!(
            matches!(result, Err(SshError::PathTraversal)),
            "expected PathTraversal, got: {result:?}"
        );
    }

    #[test]
    fn test_parse_exec_command_null_byte() {
        let result = parse_ssh_command("git-upload-pack 'owner/repo\0.git'");
        assert!(
            matches!(result, Err(SshError::PathTraversal)),
            "expected PathTraversal, got: {result:?}"
        );
    }

    #[test]
    fn test_parse_owner_repo_strip_git_suffix() {
        let result = parse_ssh_command("git-upload-pack 'owner/repo.git'").unwrap();
        assert_eq!(result.repo, "repo");
    }

    #[test]
    fn test_parse_owner_repo_no_suffix() {
        let result = parse_ssh_command("git-upload-pack 'owner/repo'").unwrap();
        assert_eq!(result.repo, "repo");
    }

    #[test]
    fn test_parse_owner_repo_nested_slash_rejected() {
        let result = parse_ssh_command("git-upload-pack 'owner/sub/repo'");
        assert!(
            matches!(result, Err(SshError::InvalidCommand)),
            "expected InvalidCommand, got: {result:?}"
        );
    }
}
