//! Claude CLI OAuth flow manager.
//!
//! Spawns `claude setup-token` via a PTY wrapper (`script`) to obtain a
//! long-lived OAuth token. The CLI outputs an OAuth URL, waits for the user
//! to paste an authentication code, then prints the resulting `sk-ant-oat01-*`
//! token to stdout.

use std::collections::HashMap;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex;
use uuid::Uuid;

/// State of a Claude CLI auth session (returned to frontend).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum AuthSessionState {
    /// Process starting, extracting URL from stdout.
    Starting,
    /// URL extracted, waiting for user to visit link and provide code.
    UrlReady { auth_url: String },
    /// Code sent to process, waiting for CLI to exchange for token.
    Verifying,
    /// Token received, flow complete.
    Completed,
    /// Process failed or timed out.
    Failed { error: String },
}

/// An active CLI auth session (owns process handle — not Clone).
#[allow(dead_code)]
struct AuthSession {
    state: AuthSessionState,
    stdin: Option<ChildStdin>,
    child: Option<Child>,
    user_id: Uuid,
    created_at: std::time::Instant,
    config_dir: Option<std::path::PathBuf>,
    /// Receives the token from the background stdout reader task.
    token_rx: Option<tokio::sync::oneshot::Receiver<String>>,
}

/// Manages active Claude CLI auth sessions.
#[derive(Default)]
pub struct CliAuthManager {
    sessions: Mutex<HashMap<Uuid, AuthSession>>,
}

impl CliAuthManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `claude setup-token` via PTY wrapper, extract the OAuth URL.
    /// Returns `(session_id, auth_url)` or error.
    ///
    /// The process stays alive waiting for the auth code on stdin.
    #[tracing::instrument(skip(self), fields(%user_id), err)]
    pub async fn start_auth(
        &self,
        user_id: Uuid,
        claude_cli_path: &str,
    ) -> Result<(Uuid, String), anyhow::Error> {
        {
            let mut sessions = self.sessions.lock().await;
            // Max 1 concurrent session per user — kill existing
            let existing: Vec<Uuid> = sessions
                .iter()
                .filter(|(_, s)| s.user_id == user_id)
                .map(|(id, _)| *id)
                .collect();
            for id in existing {
                if let Some(mut s) = sessions.remove(&id) {
                    if let Some(mut child) = s.child.take() {
                        let _ = child.kill().await;
                    }
                    cleanup_config_dir(s.config_dir.take());
                }
            }
        }

        // Create isolated config dir so CLI writes creds there
        let config_dir =
            std::env::temp_dir().join(format!("platform-claude-auth-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&config_dir).await?;

        let mut child = spawn_claude_setup_token(claude_cli_path, &config_dir)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture stdin"))?;

        // Read stdout until we find the OAuth URL (timeout 30s)
        let mut reader = BufReader::new(stdout);
        let auth_url = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            extract_oauth_url(&mut reader),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timeout waiting for OAuth URL from CLI"))?
        .map_err(|e| anyhow::anyhow!("failed to extract OAuth URL: {e}"))?;

        // Spawn background task to keep reading stdout for the token
        let (token_tx, token_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            match extract_token_from_stdout(reader).await {
                Ok(token) => {
                    tracing::info!("background reader captured OAuth token");
                    let _ = token_tx.send(token);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "background token reader ended without token");
                }
            }
        });

        let session_id = Uuid::new_v4();
        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            session_id,
            AuthSession {
                state: AuthSessionState::UrlReady {
                    auth_url: auth_url.clone(),
                },
                stdin: Some(stdin),
                child: Some(child),
                user_id,
                created_at: std::time::Instant::now(),
                config_dir: Some(config_dir),
                token_rx: Some(token_rx),
            },
        );

        tracing::info!(%session_id, %user_id, "claude auth session started");
        Ok((session_id, auth_url))
    }

    /// Send the authentication code to the waiting CLI process.
    /// Waits for the background stdout reader to capture the resulting token,
    /// validates it via `validate_oauth_token()`, and stores it on success.
    /// The token never leaves the backend.
    #[tracing::instrument(skip(self, code), fields(%session_id), err)]
    pub async fn send_code(
        &self,
        session_id: Uuid,
        code: &str,
        claude_cli_path: &str,
        pool: &sqlx::PgPool,
        master_key: &[u8; 32],
    ) -> Result<(), anyhow::Error> {
        let (user_id, token_rx) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("session not found"))?;

            // Write code to stdin (trim whitespace — copy-paste may include it)
            let code_trimmed = code.trim();
            let stdin = session
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("stdin already consumed"))?;
            tracing::info!(
                code_len = code_trimmed.len(),
                code_prefix = %&code_trimmed[..code_trimmed.len().min(6)],
                "writing auth code to CLI stdin"
            );
            stdin.write_all(code_trimmed.as_bytes()).await?;
            stdin.write_all(b"\r").await?;
            stdin.flush().await?;

            session.state = AuthSessionState::Verifying;

            let user_id = session.user_id;
            let token_rx = session
                .token_rx
                .take()
                .ok_or_else(|| anyhow::anyhow!("token receiver already consumed"))?;

            (user_id, token_rx)
        }; // Release lock while waiting

        // Wait for token from background stdout reader (timeout 30s)
        let token = tokio::time::timeout(std::time::Duration::from_secs(30), token_rx)
            .await
            .map_err(|_| anyhow::anyhow!("timeout waiting for token"))?
            .map_err(|_| anyhow::anyhow!("stdout reader task failed"))?;

        // Validate token via CLI before storing (token stays on backend)
        let valid = validate_oauth_token(claude_cli_path, &token).await?;
        if !valid {
            // Update session state to failed
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.state = AuthSessionState::Failed {
                    error: "token validation failed".into(),
                };
            }
            return Err(anyhow::anyhow!(
                "obtained token failed validation — authentication may have been rejected"
            ));
        }

        // Store validated token in cli_credentials
        crate::auth::cli_creds::store_credentials(
            pool,
            master_key,
            user_id,
            "setup_token",
            &token,
            None,
        )
        .await?;

        // Update session state + clean up process
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.state = AuthSessionState::Completed;
            if let Some(mut child) = session.child.take() {
                let _ = child.kill().await;
            }
            session.stdin.take();
        }

        tracing::info!(%session_id, %user_id, "claude auth completed and validated");
        Ok(())
    }

    /// Get current state of a session.
    pub async fn get_state(&self, session_id: Uuid) -> Option<AuthSessionState> {
        let sessions = self.sessions.lock().await;
        sessions.get(&session_id).map(|s| s.state.clone())
    }

    /// Get the user who owns a session.
    pub async fn get_owner(&self, session_id: Uuid) -> Option<Uuid> {
        let sessions = self.sessions.lock().await;
        sessions.get(&session_id).map(|s| s.user_id)
    }

    /// Cancel an auth session, kill the process, clean up temp dir.
    pub async fn cancel(&self, session_id: Uuid) {
        let mut sessions = self.sessions.lock().await;
        if let Some(mut session) = sessions.remove(&session_id) {
            if let Some(mut child) = session.child.take() {
                let _ = child.kill().await;
            }
            cleanup_config_dir(session.config_dir.take());
        }
    }

    /// Evict sessions older than 5 minutes.
    #[allow(dead_code)]
    pub async fn evict_stale(&self) {
        let mut sessions = self.sessions.lock().await;
        let threshold = std::time::Duration::from_secs(300);
        let stale: Vec<Uuid> = sessions
            .iter()
            .filter(|(_, s)| s.created_at.elapsed() > threshold)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(mut session) = sessions.remove(&id) {
                if let Some(mut child) = session.child.take() {
                    let _ = child.kill().await;
                }
                cleanup_config_dir(session.config_dir.take());
                tracing::debug!(%id, "evicted stale claude auth session");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Process spawning
// ---------------------------------------------------------------------------

/// Spawn `claude setup-token` via `script` PTY wrapper.
///
/// Sets `stty columns 500` inside the PTY so the long OAuth URL is not
/// split by line-wrapping (default PTY width is 80 columns).
fn spawn_claude_setup_token(
    claude_cli_path: &str,
    config_dir: &std::path::Path,
) -> Result<Child, anyhow::Error> {
    // Shell-escape the CLI path (handles spaces / special chars)
    let escaped_cli = claude_cli_path.replace('\'', "'\\''");
    // Widen PTY to 500 columns so the OAuth URL is never line-wrapped,
    // then exec into the CLI (replaces the shell process).
    let cmd_str = format!("stty columns 500 2>/dev/null; exec '{escaped_cli}' setup-token");

    // script provides a PTY (required by Ink TUI)
    // macOS: script -q /dev/null sh -c "<cmd>"
    // Linux: script -qc "<cmd>" /dev/null
    let child = if cfg!(target_os = "macos") {
        tokio::process::Command::new("script")
            .args(["-q", "/dev/null", "sh", "-c", &cmd_str])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env(
                "HOME",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
            )
            .env(
                "TMPDIR",
                std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()),
            )
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?
    } else {
        tokio::process::Command::new("script")
            .args(["-qc", &cmd_str, "/dev/null"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env(
                "HOME",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
            )
            .env(
                "TMPDIR",
                std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()),
            )
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?
    };

    Ok(child)
}

fn cleanup_config_dir(dir: Option<std::path::PathBuf>) {
    if let Some(dir) = dir {
        tokio::spawn(async move {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        });
    }
}

// ---------------------------------------------------------------------------
// OAuth token validation
// ---------------------------------------------------------------------------

/// Validate an OAuth token by spawning `claude --print` with
/// `CLAUDE_CODE_OAUTH_TOKEN` and checking if it authenticates.
///
/// Returns `Ok(true)` if valid, `Ok(false)` if auth failed, `Err` on spawn/timeout.
#[tracing::instrument(skip(token), err)]
pub async fn validate_oauth_token(
    claude_cli_path: &str,
    token: &str,
) -> Result<bool, anyhow::Error> {
    let config_dir =
        std::env::temp_dir().join(format!("platform-claude-validate-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&config_dir).await?;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_cli_validation(claude_cli_path, token, &config_dir),
    )
    .await;

    // Clean up temp dir
    let _ = tokio::fs::remove_dir_all(&config_dir).await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow::anyhow!("validation timed out after 30s")),
    }
}

/// Spawn `claude --print` with the OAuth token and parse NDJSON output.
async fn run_cli_validation(
    claude_cli_path: &str,
    token: &str,
    config_dir: &std::path::Path,
) -> Result<bool, anyhow::Error> {
    let output = tokio::process::Command::new(claude_cli_path)
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--max-turns",
            "1",
            "hi",
        ])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .env(
            "TMPDIR",
            std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()),
        )
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env("CLAUDE_CODE_OAUTH_TOKEN", token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    tracing::info!(
        exit_code,
        stdout_len = stdout.len(),
        stderr_len = stderr.len(),
        cli_path = %claude_cli_path,
        "claude oauth validation result"
    );

    // Log stdout/stderr for debugging
    if !stdout.is_empty() {
        let preview: String = stdout.chars().take(2000).collect();
        tracing::debug!(stdout = %preview, "claude validation stdout");
    }
    if !stderr.is_empty() {
        let preview: String = stderr.chars().take(2000).collect();
        tracing::warn!(stderr = %preview, "claude validation stderr");
    }

    Ok(parse_validation_ndjson(&stdout))
}

/// Parse the NDJSON output from `claude --print` to determine if auth succeeded.
fn parse_validation_ndjson(stdout: &str) -> bool {
    let mut saw_init = false;
    let mut line_count = 0;
    let mut json_count = 0;

    for line in stdout.lines() {
        line_count += 1;
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        json_count += 1;

        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let msg_subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
        // error field may be a string ("authentication_failed") or an object
        // ({"type": "authentication_failed", "message": "..."})
        let msg_error = v
            .get("error")
            .and_then(|e| {
                e.as_str()
                    .map(String::from)
                    .or_else(|| e.get("type").and_then(|t| t.as_str()).map(String::from))
            })
            .unwrap_or_default();

        tracing::debug!(
            line_count,
            json_count,
            msg_type,
            msg_subtype,
            %msg_error,
            "validation: parsed NDJSON line"
        );

        if msg_error == "authentication_failed" {
            tracing::warn!("validation: found authentication_failed error");
            return false;
        }

        if msg_type == "system" && msg_subtype == "init" {
            saw_init = true;
            tracing::info!("validation: saw system.init — auth succeeded");
        }

        if msg_type == "result" {
            let is_error = v.get("is_error").and_then(serde_json::Value::as_bool);
            tracing::info!(is_error = ?is_error, saw_init, "validation: got result line");
            if is_error == Some(false) || saw_init {
                return true;
            }
            return false;
        }
    }

    tracing::info!(
        line_count,
        json_count,
        saw_init,
        "validation: finished parsing all lines"
    );

    if saw_init {
        tracing::info!("no result line but init was seen — treating as valid auth");
        return true;
    }

    tracing::warn!(
        stdout_len = stdout.len(),
        stdout_preview = %stdout.chars().take(1000).collect::<String>(),
        "validation: no init and no result found — returning false"
    );
    false
}

// ---------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------

/// Read PTY stdout, strip ANSI escape codes, find the OAuth URL.
async fn extract_oauth_url(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<String, anyhow::Error> {
    let mut buf = Vec::new();
    let mut accumulated = String::new();

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Err(anyhow::anyhow!("CLI exited before producing URL"));
        }
        let line = String::from_utf8_lossy(&buf);
        let clean = strip_ansi_escapes(&line);
        accumulated.push_str(&clean);

        if let Some(url) = find_oauth_url(&accumulated) {
            return Ok(url);
        }
    }
}

/// Continue reading stdout after URL was found, looking for the token.
///
/// Uses raw `read()` instead of `read_until(b'\n')` because the Claude CLI's
/// Ink TUI may render the token output using carriage returns (`\r`) and ANSI
/// cursor movement without newlines — `read_until(b'\n')` would block forever.
async fn extract_token_from_stdout(
    mut reader: BufReader<tokio::process::ChildStdout>,
) -> Result<String, anyhow::Error> {
    use tokio::io::AsyncReadExt;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            // Log accumulated output for debugging before returning error
            let text = String::from_utf8_lossy(&raw);
            let clean = strip_ansi_escapes(&text);
            tracing::warn!(
                raw_len = raw.len(),
                clean_preview = %clean.chars().take(800).collect::<String>(),
                "CLI stdout EOF — no token found"
            );
            return Err(anyhow::anyhow!(
                "CLI exited before producing token (read {} bytes)",
                raw.len()
            ));
        }
        raw.extend_from_slice(&buf[..n]);

        // Strip ANSI from the ENTIRE accumulated output (not per-chunk,
        // since escape sequences can span chunk boundaries).  The Ink TUI
        // inserts cursor-movement codes (`\x1b[1C`) between characters,
        // which breaks literal `sk-ant-oat` matching on raw bytes.
        let text = String::from_utf8_lossy(&raw);
        let clean = strip_ansi_escapes(&text);

        tracing::debug!(
            chunk_bytes = n,
            total_bytes = raw.len(),
            clean_tail = %clean.chars().rev().take(120).collect::<String>().chars().rev().collect::<String>(),
            "CLI stdout chunk"
        );

        if let Some(token) = find_oauth_token(&clean) {
            tracing::info!(
                token_len = token.len(),
                token_prefix = %&token[..token.len().min(20)],
                "extracted OAuth token from CLI stdout"
            );
            return Ok(token);
        }
    }
}

/// Strip ANSI escape sequences from terminal output.
fn strip_ansi_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC sequence — skip until terminator
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    // CSI sequence: ESC [ ... final_byte (letter)
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() || ch == 'h' || ch == 'l' {
                            break;
                        }
                    }
                } else if next == ']' {
                    // OSC sequence: ESC ] ... BEL or ST
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '\x07' || ch == '\\' {
                            break;
                        }
                    }
                } else {
                    chars.next(); // skip one char after ESC
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Find OAuth URL in text (starts with `https://claude.ai/oauth/authorize?`).
///
/// Handles PTY line-wrapping: single `\r\n` within the URL is treated as a
/// wrap artifact and skipped; a blank line (`\n\n`) or a non-URL character
/// terminates the URL.
fn find_oauth_url(text: &str) -> Option<String> {
    // Try multiple known URL patterns (claude.com/cai/oauth for v2.1+, claude.ai/oauth for older)
    let markers = [
        "https://claude.com/cai/oauth/authorize?",
        "https://claude.ai/oauth/authorize?",
    ];
    let (marker_len, start) = markers
        .iter()
        .find_map(|m| text.find(m).map(|pos| (m.len(), pos)))?;
    let rest = &text[start..];
    let bytes = rest.as_bytes();

    let mut url = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\r' {
            // Skip carriage returns (PTY artifact)
            i += 1;
            continue;
        }
        if b == b'\n' {
            // Count consecutive newlines (ignoring \r)
            let mut j = i;
            let mut nl_count = 0;
            while j < bytes.len() && (bytes[j] == b'\n' || bytes[j] == b'\r') {
                if bytes[j] == b'\n' {
                    nl_count += 1;
                }
                j += 1;
            }
            if nl_count >= 2 {
                // Blank line → URL is done
                break;
            }
            // Single newline → PTY line wrap, skip it
            i = j;
            continue;
        }
        let c = b as char;
        if is_url_char(c) {
            url.push(c);
            i += 1;
        } else {
            break;
        }
    }

    if url.len() > marker_len {
        Some(url)
    } else {
        None
    }
}

/// Characters valid in a URL (RFC 3986 unreserved + reserved + percent-encoding).
fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Find `sk-ant-oat...` token in text (raw or ANSI-stripped).
///
/// Uses substring search to find the marker, then extracts token characters
/// (alphanumeric, `-`, `_`). Treats ESC (`\x1b`) and other control characters
/// as terminators so that adjacent ANSI cursor-positioning sequences correctly
/// delimit the token even in raw PTY output.
fn find_oauth_token(text: &str) -> Option<String> {
    let marker = "sk-ant-oat";
    let start = text.find(marker)?;
    let rest = &text[start..];
    // Stop at ESC (ANSI sequence start), control chars, spaces, or any
    // character that isn't part of a base64url token.
    let end = rest
        .find(|c: char| c == '\x1b' || (!c.is_ascii_alphanumeric() && c != '-' && c != '_'))
        .unwrap_or(rest.len());
    let token = &rest[..end];
    if token.len() > 20 {
        Some(token.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // Pure unit tests — no I/O, testing private parsing/extraction helpers.
    // These are correctly placed in the unit tier.
    // =====================================================================

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        let input = "\x1b[?2026h\x1b[32mHello\x1b[0m world\x1b[?2026l";
        let result = strip_ansi_escapes(input);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        let input = "Hello world";
        assert_eq!(strip_ansi_escapes(input), "Hello world");
    }

    #[test]
    fn strip_ansi_handles_empty_string() {
        assert_eq!(strip_ansi_escapes(""), "");
    }

    #[test]
    fn strip_ansi_handles_complex_output() {
        // Simulated PTY output with mixed ANSI codes
        let input = "\x1b[?2026h\x1b[1mBrowser didn't open?\x1b[0m Use the url below\n";
        let result = strip_ansi_escapes(input);
        assert!(result.contains("Browser didn't open?"));
        assert!(result.contains("Use the url below"));
    }

    #[test]
    fn find_oauth_url_extracts_url() {
        let text = "Browser didn't open? Use the url below\nhttps://claude.ai/oauth/authorize?code=true&client_id=abc&state=xyz\n\nPaste code here";
        let url = find_oauth_url(text).unwrap();
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("client_id=abc"));
        assert!(url.contains("state=xyz"));
    }

    #[test]
    fn find_oauth_url_extracts_claude_com_url() {
        let text = "Browser didn't open? Use the url below to sign in (c to copy)\nhttps://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a&state=xyz\n\nPaste code here";
        let url = find_oauth_url(text).unwrap();
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a"));
    }

    #[test]
    fn find_oauth_url_returns_none_for_no_url() {
        let text = "No URL here, just some text";
        assert!(find_oauth_url(text).is_none());
    }

    #[test]
    fn find_oauth_url_handles_url_at_end_of_text() {
        let text = "Visit: https://claude.ai/oauth/authorize?code=true";
        let url = find_oauth_url(text).unwrap();
        assert_eq!(url, "https://claude.ai/oauth/authorize?code=true");
    }

    #[test]
    fn find_oauth_url_handles_claude_com_at_end() {
        let text = "Visit: https://claude.com/cai/oauth/authorize?code=true";
        let url = find_oauth_url(text).unwrap();
        assert_eq!(url, "https://claude.com/cai/oauth/authorize?code=true");
    }

    #[test]
    fn find_oauth_url_handles_pty_wrapped_url() {
        // Simulate PTY output where URL wraps at 80 columns with \r\n
        let text = "Browser didn't open?\n\
            https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-59\r\n\
            44d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.c\r\n\
            laude.com&scope=user%3Ainference&state=abc123\r\n\
            \r\n\
            Paste code here:";
        let url = find_oauth_url(text).unwrap();
        assert!(url.contains("9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("&state=abc123"));
        assert!(!url.contains("Paste"));
    }

    #[test]
    fn find_oauth_url_handles_claude_com_pty_wrapped() {
        let text = "Browser didn't open? Use the url below to sign in (c to copy)\n\
            https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-59\r\n\
            44d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com\r\n\
            %2Foauth%2Fcode%2Fcallback&scope=user%3Ainference&state=abc123\r\n\
            \r\n\
            Paste code here if prompted >";
        let url = find_oauth_url(text).unwrap();
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
        assert!(url.contains("9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(!url.contains("Paste"));
    }

    #[test]
    fn find_oauth_token_extracts_token() {
        let text =
            "Your OAuth token (valid for 1 year):\nsk-ant-oat01-FAKE_TEST_TOKEN_aabbccdd1122334455";
        let token = find_oauth_token(text).unwrap();
        assert!(token.starts_with("sk-ant-oat01-"));
    }

    #[test]
    fn find_oauth_token_returns_none_for_no_token() {
        let text = "No token here";
        assert!(find_oauth_token(text).is_none());
    }

    #[test]
    fn find_oauth_token_ignores_short_prefix() {
        // "sk-ant-oat" alone is too short (< 20 chars)
        let text = "sk-ant-oat";
        assert!(find_oauth_token(text).is_none());
    }

    #[test]
    fn find_oauth_token_stops_at_esc() {
        // Raw PTY output: ANSI cursor-positioning between token and next text
        let text = "sk-ant-oat01-FAKE_TEST_aaaa-bbbb-cccc-dddd-eeee1111222233\x1b[28;1HStore";
        let token = find_oauth_token(text).unwrap();
        assert_eq!(
            token,
            "sk-ant-oat01-FAKE_TEST_aaaa-bbbb-cccc-dddd-eeee1111222233"
        );
    }

    #[test]
    fn find_oauth_token_from_real_output() {
        // Works with both ANSI-stripped and raw text
        let raw = "\x1b[32m✓\x1b[0m Long-lived authentication token created successfully!\n\
                    Your OAuth token (valid for 1 year):\n\
                    sk-ant-oat01-FAKE_TEST_xxxx-yyyy-zzzz-1234567890abcdefghijklmnopqrstuvwxyz-AABBCCDD";
        // Raw text — ESC before token is fine since find starts at marker
        let token = find_oauth_token(raw).unwrap();
        assert!(token.starts_with("sk-ant-oat01-"));
        assert!(token.len() > 40);

        // ANSI-stripped text also works
        let clean = strip_ansi_escapes(raw);
        let token2 = find_oauth_token(&clean).unwrap();
        assert_eq!(token, token2);
    }

    #[test]
    fn find_oauth_token_merged_with_adjacent_text() {
        // When preceding text is adjacent (no separator), find still works via marker search
        let text = "year):sk-ant-oat01-FAKE_TEST_aaaa-bbbb-cccc-dddd-eeee1111222233-FAKE_TEST_suffix_aabbccddeeff00112233 rest";
        let token = find_oauth_token(text).unwrap();
        assert_eq!(
            token,
            "sk-ant-oat01-FAKE_TEST_aaaa-bbbb-cccc-dddd-eeee1111222233-FAKE_TEST_suffix_aabbccddeeff00112233"
        );
    }

    #[test]
    fn find_oauth_token_from_ink_tui_raw_output() {
        // Simulate real Ink TUI raw PTY output: ANSI cursor-positioning sequences
        // sit directly between the token and the next line of text.
        // find_oauth_token works on raw text — ESC byte terminates the token.
        let raw = "\x1b[22;1H\x1b[32m✓\x1b[0m Long-lived authentication token created successfully!\x1b[24;1H\
                   Your OAuth token (valid for 1 year):                                    \x1b[26;1H\
                   sk-ant-oat01-FAKE_TEST_aaaa-bbbb-cccc-dddd-eeee1111222233-FAKE_TEST_suffix_aabbccddeeff00112233\
                   \x1b[28;1HStore this token securely.";
        let token = find_oauth_token(raw).unwrap();
        assert!(token.starts_with("sk-ant-oat01-"));
        assert!(token.ends_with("aabbccddeeff00112233"));
    }

    #[test]
    fn cli_auth_manager_new() {
        let _manager = CliAuthManager::new();
    }

    // =====================================================================
    // parse_validation_ndjson tests — pure function, no I/O
    // =====================================================================

    #[test]
    fn parse_validation_ndjson_auth_success() {
        // Valid init + result with is_error=false → true
        let output = r#"{"type":"system","subtype":"init","session_id":"abc"}
{"type":"result","is_error":false,"result":"Hello!"}"#;
        assert!(parse_validation_ndjson(output));
    }

    #[test]
    fn parse_validation_ndjson_auth_failed() {
        // authentication_failed error → false
        let output = r#"{"error":"authentication_failed","message":"Invalid token"}"#;
        assert!(!parse_validation_ndjson(output));
    }

    #[test]
    fn parse_validation_ndjson_result_is_error() {
        // result with is_error=true (no init) → false
        let output = r#"{"type":"result","is_error":true,"result":"error"}"#;
        assert!(!parse_validation_ndjson(output));
    }

    #[test]
    fn parse_validation_ndjson_empty_output() {
        // Empty output → false
        assert!(!parse_validation_ndjson(""));
    }

    #[test]
    fn parse_validation_ndjson_no_json_lines() {
        // Non-JSON text → false
        let output = "this is not json\nneither is this\n";
        assert!(!parse_validation_ndjson(output));
    }

    // =====================================================================
    // PTY subprocess tests — spawn real processes + filesystem I/O.
    //
    // Tier exception: these tests spawn real subprocesses (mock CLI via
    // macOS `script` PTY wrapper) and perform filesystem I/O (temp dirs,
    // writing mock scripts). By the decision tree they would be integration
    // tier. However, they remain in the unit tier (#[cfg(test)] mod tests)
    // because they test private functions (`spawn_claude_setup_token`,
    // `extract_oauth_url`, `extract_token_from_stdout`) that are not
    // accessible from external `tests/` binaries. Moving them out would
    // require making those functions `pub`, leaking implementation details
    // into the public API.
    // =====================================================================

    /// Create a temporary mock CLI script that behaves like `claude setup-token`:
    /// 1. Prints banner + OAuth URL on stdout
    /// 2. Reads one line from stdin (the auth code)
    /// 3. Prints success message + OAuth token on stdout
    fn create_mock_cli(dir: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("mock-claude");
        std::fs::write(
            &script,
            r#"#!/bin/bash
# Simulate claude setup-token Ink TUI output
echo "Welcome to Claude Code"
echo "Browser didn't open? Use the url below to sign in"
echo "https://claude.ai/oauth/authorize?code=true&client_id=test-client&state=test-state"
echo ""
echo "Paste code here if prompted >"

# Read auth code from stdin (one line)
read -r code

# Simulate successful token exchange
echo ""
echo "Long-lived authentication token created successfully!"
echo "Your OAuth token (valid for 1 year):"
echo "sk-ant-oat01-MockTestToken_abcdefghijklmnopqrstuvwxyz1234567890-AABBCC"
echo "Store this token securely."
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    /// Tier exception: spawns real subprocess + filesystem I/O (see block comment above).
    #[tokio::test]
    async fn pty_flow_url_extraction() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_cli = create_mock_cli(tmp.path());
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let mut child = spawn_claude_setup_token(mock_cli.to_str().unwrap(), &config_dir).unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);

        let url = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            extract_oauth_url(&mut reader),
        )
        .await
        .expect("URL extraction timed out")
        .expect("URL extraction failed");

        assert!(
            url.starts_with("https://claude.ai/oauth/authorize?"),
            "unexpected URL: {url}"
        );
        assert!(url.contains("client_id=test-client"));

        child.kill().await.ok();
    }

    /// Tier exception: spawns real subprocess + filesystem I/O (see block comment above).
    #[tokio::test]
    async fn pty_flow_full_code_to_token() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_cli = create_mock_cli(tmp.path());
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let mut child = spawn_claude_setup_token(mock_cli.to_str().unwrap(), &config_dir).unwrap();

        let stdout = child.stdout.take().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut reader = BufReader::new(stdout);

        // Phase 1: Extract URL
        let url = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            extract_oauth_url(&mut reader),
        )
        .await
        .expect("URL timed out")
        .expect("URL failed");
        assert!(url.contains("claude.ai/oauth/authorize"));

        // Phase 2: Start background token reader
        let (token_tx, token_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            match extract_token_from_stdout(reader).await {
                Ok(token) => {
                    let _ = token_tx.send(token);
                }
                Err(e) => {
                    panic!("token extraction failed: {e}");
                }
            }
        });

        // Phase 3: Write auth code to stdin (same as send_code does)
        stdin.write_all(b"test-auth-code-12345\r").await.unwrap();
        stdin.flush().await.unwrap();

        // Phase 4: Wait for token
        let token = tokio::time::timeout(std::time::Duration::from_secs(5), token_rx)
            .await
            .expect("token timed out")
            .expect("token rx failed");

        assert!(
            token.starts_with("sk-ant-oat01-"),
            "unexpected token: {token}"
        );
        assert!(
            token.contains("MockTestToken"),
            "token should contain MockTestToken: {token}"
        );

        child.kill().await.ok();
    }

    /// Tier exception: spawns real subprocess + filesystem I/O (see block comment above).
    #[tokio::test]
    async fn pty_flow_manager_start_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_cli = create_mock_cli(tmp.path());

        let manager = CliAuthManager::new();
        let user_id = Uuid::new_v4();

        let (session_id, url) = manager
            .start_auth(user_id, mock_cli.to_str().unwrap())
            .await
            .expect("start_auth failed");

        assert!(url.contains("claude.ai/oauth/authorize"));

        // Verify session state
        let state = manager.get_state(session_id).await.unwrap();
        assert!(matches!(state, AuthSessionState::UrlReady { .. }));

        // Clean up
        manager.cancel(session_id).await;
    }

    // =====================================================================
    // Additional parse_validation_ndjson coverage
    // =====================================================================

    #[test]
    fn parse_validation_ndjson_init_only_no_result() {
        // init line seen but no result line → saw_init is true → returns true
        let output = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert!(parse_validation_ndjson(output));
    }

    #[test]
    fn parse_validation_ndjson_result_error_after_init() {
        // init + result with is_error=true → saw_init is true from init line,
        // result checks if is_error == Some(false) || saw_init → true
        let output = r#"{"type":"system","subtype":"init","session_id":"abc"}
{"type":"result","is_error":true,"result":"some error"}"#;
        assert!(parse_validation_ndjson(output));
    }

    #[test]
    fn parse_validation_ndjson_result_without_is_error_and_no_init() {
        // result line without is_error field (and no init) → is_error == None
        // Neither is_error == Some(false) nor saw_init → false
        let output = r#"{"type":"result","result":"something"}"#;
        assert!(!parse_validation_ndjson(output));
    }

    #[test]
    fn parse_validation_ndjson_mixed_json_and_non_json() {
        // Lines with non-JSON interspersed — should skip non-JSON
        let output = "not json at all\n{\"type\":\"system\",\"subtype\":\"init\"}\nmore text\n{\"type\":\"result\",\"is_error\":false}";
        assert!(parse_validation_ndjson(output));
    }

    #[test]
    fn parse_validation_ndjson_auth_failed_before_init() {
        // auth_failed appears before any init → false (early return)
        let output = r#"{"error":"authentication_failed","message":"bad token"}
{"type":"system","subtype":"init"}"#;
        assert!(!parse_validation_ndjson(output));
    }

    // =====================================================================
    // Additional strip_ansi_escapes coverage
    // =====================================================================

    #[test]
    fn strip_ansi_handles_osc_sequences() {
        // OSC sequence: ESC ] ... BEL
        let input = "\x1b]0;window title\x07Hello";
        let result = strip_ansi_escapes(input);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn strip_ansi_handles_esc_with_unknown_next_char() {
        // ESC followed by a non-[ non-] char → skip ESC + that char
        let input = "\x1bMHello";
        let result = strip_ansi_escapes(input);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn strip_ansi_handles_esc_at_end() {
        // ESC at end of string with no following char → skipped
        let input = "Hello\x1b";
        let result = strip_ansi_escapes(input);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn strip_ansi_handles_multiple_csi() {
        let input = "\x1b[1m\x1b[31mRed Bold\x1b[0m Normal";
        let result = strip_ansi_escapes(input);
        assert_eq!(result, "Red Bold Normal");
    }

    // =====================================================================
    // Additional find_oauth_url coverage
    // =====================================================================

    #[test]
    fn find_oauth_url_too_short_returns_none() {
        // URL exactly equal to the marker (no query params) → length <= marker_len → None
        let text = "https://claude.ai/oauth/authorize?";
        assert!(find_oauth_url(text).is_none());
    }

    #[test]
    fn find_oauth_url_handles_cr_in_url() {
        // \r chars within URL should be skipped
        let text = "https://claude.ai/oauth/authorize?code=true\r&state=abc";
        let url = find_oauth_url(text).unwrap();
        assert_eq!(url, "https://claude.ai/oauth/authorize?code=true&state=abc");
    }

    #[test]
    fn find_oauth_url_stops_at_non_url_char() {
        // Space terminates URL
        let text = "https://claude.ai/oauth/authorize?code=true next text";
        let url = find_oauth_url(text).unwrap();
        assert_eq!(url, "https://claude.ai/oauth/authorize?code=true");
    }

    // =====================================================================
    // Additional find_oauth_token coverage
    // =====================================================================

    #[test]
    fn find_oauth_token_stops_at_space() {
        let text = "sk-ant-oat01-FAKE_LONG_TOKEN_abcdef1234567890 next";
        let token = find_oauth_token(text).unwrap();
        assert_eq!(token, "sk-ant-oat01-FAKE_LONG_TOKEN_abcdef1234567890");
    }

    #[test]
    fn find_oauth_token_stops_at_newline() {
        let text = "sk-ant-oat01-FAKE_LONG_TOKEN_abcdef1234567890\nnext line";
        let token = find_oauth_token(text).unwrap();
        assert_eq!(token, "sk-ant-oat01-FAKE_LONG_TOKEN_abcdef1234567890");
    }

    #[test]
    fn find_oauth_token_exactly_21_chars() {
        // Just above the 20 char threshold (token.len() > 20)
        let text = "sk-ant-oat01-ABCDEFGH";
        // "sk-ant-oat01-ABCDEFGH" is 21 chars
        let token = find_oauth_token(text).unwrap();
        assert_eq!(token, "sk-ant-oat01-ABCDEFGH");
    }

    #[test]
    fn find_oauth_token_exactly_20_chars() {
        // At the threshold (token.len() > 20 required, so 20 is too short)
        let text = "sk-ant-oat01-ABCDEFG";
        // "sk-ant-oat01-ABCDEFG" is 20 chars
        assert!(find_oauth_token(text).is_none());
    }

    // =====================================================================
    // is_url_char coverage
    // =====================================================================

    #[test]
    fn is_url_char_basic() {
        assert!(is_url_char('a'));
        assert!(is_url_char('Z'));
        assert!(is_url_char('0'));
        assert!(is_url_char('-'));
        assert!(is_url_char('_'));
        assert!(is_url_char('.'));
        assert!(is_url_char('~'));
        assert!(is_url_char(':'));
        assert!(is_url_char('/'));
        assert!(is_url_char('?'));
        assert!(is_url_char('#'));
        assert!(is_url_char('&'));
        assert!(is_url_char('='));
        assert!(is_url_char('%'));
        assert!(!is_url_char(' '));
        assert!(!is_url_char('\n'));
        assert!(!is_url_char('\x00'));
        assert!(!is_url_char('<'));
        assert!(!is_url_char('>'));
    }

    // =====================================================================
    // CliAuthManager: concurrent session replacement
    // =====================================================================

    #[tokio::test]
    async fn cli_auth_manager_replaces_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_cli = create_mock_cli(tmp.path());

        let manager = CliAuthManager::new();
        let user_id = Uuid::new_v4();

        // Start first session
        let (session_id_1, url_1) = manager
            .start_auth(user_id, mock_cli.to_str().unwrap())
            .await
            .expect("first start_auth failed");
        assert!(url_1.contains("claude.ai/oauth/authorize"));

        // Start second session for same user — should kill the first
        let (session_id_2, url_2) = manager
            .start_auth(user_id, mock_cli.to_str().unwrap())
            .await
            .expect("second start_auth failed");
        assert!(url_2.contains("claude.ai/oauth/authorize"));
        assert_ne!(session_id_1, session_id_2);

        // First session should be gone
        assert!(manager.get_state(session_id_1).await.is_none());
        // Second session should exist
        assert!(manager.get_state(session_id_2).await.is_some());

        // Clean up
        manager.cancel(session_id_2).await;
    }

    #[tokio::test]
    async fn cli_auth_manager_get_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_cli = create_mock_cli(tmp.path());

        let manager = CliAuthManager::new();
        let user_id = Uuid::new_v4();

        let (session_id, _) = manager
            .start_auth(user_id, mock_cli.to_str().unwrap())
            .await
            .expect("start_auth failed");

        let owner = manager.get_owner(session_id).await;
        assert_eq!(owner, Some(user_id));

        // Unknown session returns None
        assert!(manager.get_owner(Uuid::new_v4()).await.is_none());

        manager.cancel(session_id).await;
    }

    #[tokio::test]
    async fn cli_auth_manager_cancel_nonexistent_is_noop() {
        let manager = CliAuthManager::new();
        // Should not panic
        manager.cancel(Uuid::new_v4()).await;
    }
}
