// Forked from src/agent/claude_cli/transport.rs — keep in sync manually

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::control::ControlRequest;
use crate::error::CliError;
use crate::messages::{parse_cli_message, CliMessage, CliUserInput};

/// Subprocess transport for the Claude CLI NDJSON protocol.
///
/// Spawns `claude` as a child process with `--input-format stream-json
/// --output-format stream-json`. Provides methods to send/receive NDJSON
/// messages over stdin/stdout.
pub struct SubprocessTransport {
    pub(crate) child: Child,
    pub(crate) stdin: Mutex<BufWriter<ChildStdin>>,
    pub(crate) stdout: Mutex<BufReader<ChildStdout>>,
    pub(crate) stderr_task: Option<JoinHandle<String>>,
    pub(crate) session_id: Mutex<Option<String>>,
    pub(crate) alive: std::sync::atomic::AtomicBool,
}

/// Options for spawning the Claude CLI subprocess.
///
/// All fields are optional — reasonable defaults are used when absent.
#[derive(Default)]
pub struct CliSpawnOptions {
    /// Override CLI binary path.
    pub cli_path: Option<PathBuf>,
    /// Working directory for the CLI process.
    pub cwd: Option<PathBuf>,
    /// `--model` flag.
    pub model: Option<String>,
    /// `--system-prompt` flag.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` flag.
    pub append_system_prompt: Option<String>,
    /// `--allowedTools` flag (comma-separated tool names).
    pub allowed_tools: Option<Vec<String>>,
    /// `--permission-mode` flag (e.g. "bypassPermissions").
    pub permission_mode: Option<String>,
    /// `--max-turns` flag.
    pub max_turns: Option<u32>,
    /// `--resume <session-id>` to continue a previous conversation.
    pub resume_session: Option<String>,
    /// `--mcp-config <path>` for MCP server configuration.
    pub mcp_config: Option<PathBuf>,
    /// `--include-partial-messages` for streaming partial tokens.
    pub include_partial: bool,
    /// `CLAUDE_CONFIG_DIR` env var.
    pub config_dir: Option<PathBuf>,
    /// `CLAUDE_CODE_OAUTH_TOKEN` env var (subscription auth).
    pub oauth_token: Option<String>,
    /// `ANTHROPIC_API_KEY` env var (API key auth — fallback).
    pub anthropic_api_key: Option<String>,
    /// Additional environment variables to pass to the subprocess.
    pub extra_env: Vec<(String, String)>,
    /// Initial prompt — passed as `--print <prompt>` (positional arg).
    /// Required for `--input-format stream-json` to take effect.
    pub initial_prompt: Option<String>,
    /// Use `env_clear()` + whitelist for security isolation (pod mode).
    /// When false, inherits parent env (REPL/local mode — needed for
    /// config-dir OAuth credentials to work).
    pub isolate_env: bool,
}

impl SubprocessTransport {
    /// Spawn the Claude CLI as a subprocess.
    ///
    /// **Security:** Uses `Command::env_clear()` then adds ONLY whitelisted vars
    /// (PATH, HOME, TMPDIR, auth vars, `CLAUDE_CONFIG_DIR`, `extra_env`).
    /// This prevents leaking `DATABASE_URL`, `PLATFORM_MASTER_KEY`, etc.
    #[allow(clippy::needless_pass_by_value)]
    pub fn spawn(opts: CliSpawnOptions) -> Result<Self, CliError> {
        let cli_path = find_claude_cli(opts.cli_path.as_deref())?;
        let args = build_args(&opts);
        let env_vars = build_env(&opts);

        let mut cmd = tokio::process::Command::new(&cli_path);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if opts.isolate_env {
            // Pod mode: clear env then add only whitelisted vars
            cmd.env_clear();
            for (key, value) in &env_vars {
                cmd.env(key, value);
            }
        } else {
            // REPL mode: inherit parent env, overlay explicit vars
            for (key, value) in &env_vars {
                cmd.env(key, value);
            }
        }

        if let Some(ref cwd) = opts.cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd.spawn().map_err(CliError::SpawnFailed)?;

        let stdin = child.stdin.take().ok_or(CliError::NotRunning)?;
        let stdout = child.stdout.take().ok_or(CliError::NotRunning)?;
        let stderr = child.stderr.take();

        // Spawn a task to capture stderr for error reporting
        let stderr_task = stderr.map(|stderr| {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut collected = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.is_empty() {
                        eprintln!("[stderr] {}", line);
                        if collected.len() < 4096 && !collected.is_empty() {
                            collected.push('\n');
                        }
                        if collected.len() < 4096 {
                            collected.push_str(&line);
                        }
                    }
                }
                collected
            })
        });

        Ok(Self {
            child,
            stdin: Mutex::new(BufWriter::new(stdin)),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// Send a user text message to the CLI via stdin.
    pub async fn send_message(&self, content: &str) -> Result<(), CliError> {
        let input = CliUserInput::text(content);
        self.write_json(&input).await
    }

    /// Send structured content (multi-part, images) via stdin.
    pub async fn send_structured(&self, content: serde_json::Value) -> Result<(), CliError> {
        let input = CliUserInput::structured(content);
        self.write_json(&input).await
    }

    /// Read the next NDJSON message from stdout.
    ///
    /// Returns `Ok(None)` when stdout closes (process exited).
    /// Skips unknown message types and empty lines.
    pub async fn recv(&self) -> Result<Option<CliMessage>, CliError> {
        let mut stdout = self.stdout.lock().await;
        loop {
            let mut line = String::new();
            let bytes_read = stdout
                .read_line(&mut line)
                .await
                .map_err(CliError::StdoutRead)?;

            if bytes_read == 0 {
                self.alive
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return Ok(None);
            }

            match parse_cli_message(&line) {
                Ok(Some(msg)) => {
                    // Track session ID from system init
                    if let CliMessage::System(ref sys) = msg {
                        let mut sid = self.session_id.lock().await;
                        *sid = Some(sys.session_id.clone());
                    }
                    return Ok(Some(msg));
                }
                Ok(None) => {
                    // Unknown type or empty line — skip
                }
                Err(e) => {
                    // Truncate to avoid logging sensitive data from malformed CLI output
                    let trimmed = line.trim();
                    let preview = &trimmed[..trimmed.len().min(200)];
                    eprintln!("[warn] skipping invalid NDJSON: {preview}");
                    let _ = e; // consumed for the log message above
                }
            }
        }
    }

    /// Send a control request (interrupt, `set_model`, etc.).
    pub async fn send_control(&self, request: ControlRequest) -> Result<(), CliError> {
        self.write_json(&request).await
    }

    /// Get the CLI session ID (available after receiving the System init message).
    pub async fn session_id(&self) -> Option<String> {
        self.session_id.lock().await.clone()
    }

    /// Kill the subprocess.
    pub async fn kill(&mut self) -> Result<(), CliError> {
        self.alive
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.child
            .kill()
            .await
            .map_err(|e| CliError::SessionError(format!("failed to kill CLI process: {e}")))
    }

    /// Check if the subprocess is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Wait for the process to exit and return the exit code + stderr.
    pub async fn wait(mut self) -> Result<(i32, String), CliError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| CliError::SessionError(format!("wait failed: {e}")))?;

        self.alive
            .store(false, std::sync::atomic::Ordering::Relaxed);

        let stderr = if let Some(task) = self.stderr_task.take() {
            task.await.unwrap_or_else(|e| {
                eprintln!("[warn] stderr capture task panicked: {}", e);
                String::new()
            })
        } else {
            String::new()
        };

        Ok((status.code().unwrap_or(-1), stderr))
    }

    /// Write a JSON value followed by newline to stdin.
    async fn write_json(&self, value: &impl serde::Serialize) -> Result<(), CliError> {
        if !self.is_alive() {
            return Err(CliError::NotRunning);
        }
        let mut json = serde_json::to_string(value).map_err(|e| {
            CliError::StdinWrite(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        json.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(json.as_bytes())
            .await
            .map_err(CliError::StdinWrite)?;
        stdin.flush().await.map_err(CliError::StdinWrite)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CLI discovery
// ---------------------------------------------------------------------------

/// Find the `claude` CLI binary.
///
/// Priority:
/// 1. Explicit path from `CliSpawnOptions`
/// 2. `CLAUDE_CLI_PATH` env var
/// 3. PATH lookup via `which`
/// 4. Common npm global install paths
/// 5. `/usr/local/bin/claude`
fn find_claude_cli(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    // 1. Explicit path
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(CliError::CliNotFound);
    }

    // 2. CLAUDE_CLI_PATH env var
    if let Ok(path) = std::env::var("CLAUDE_CLI_PATH") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
    }

    // 3. PATH lookup
    if let Ok(output) = std::process::Command::new("which").arg("claude").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    // 4. Common npm global paths
    let npm_paths = ["/usr/local/bin/claude", "/usr/bin/claude"];
    for path in &npm_paths {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }

    Err(CliError::CliNotFound)
}

/// Build CLI arguments from spawn options.
///
/// Always uses `--input-format stream-json --output-format stream-json`.
/// The initial prompt is NOT passed as a CLI arg — it must be sent as a
/// JSON user message on stdin after spawn (see `send_message`).
pub(crate) fn build_args(opts: &CliSpawnOptions) -> Vec<String> {
    let mut args = vec![
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
    ];

    if let Some(ref model) = opts.model {
        args.push("--model".to_owned());
        args.push(model.clone());
    }

    if let Some(ref system_prompt) = opts.system_prompt {
        args.push("--system-prompt".to_owned());
        args.push(system_prompt.clone());
    }

    if let Some(ref append) = opts.append_system_prompt {
        args.push("--append-system-prompt".to_owned());
        args.push(append.clone());
    }

    if let Some(ref tools) = opts.allowed_tools {
        args.push("--allowedTools".to_owned());
        args.push(tools.join(","));
    }

    if let Some(ref mode) = opts.permission_mode {
        args.push("--permission-mode".to_owned());
        args.push(mode.clone());
    }

    if let Some(max_turns) = opts.max_turns {
        args.push("--max-turns".to_owned());
        args.push(max_turns.to_string());
    }

    if let Some(ref session_id) = opts.resume_session {
        args.push("--resume".to_owned());
        args.push(session_id.clone());
    }

    if let Some(ref path) = opts.mcp_config {
        args.push("--mcp-config".to_owned());
        args.push(path.display().to_string());
    }

    if opts.include_partial {
        args.push("--include-partial-messages".to_owned());
    }

    args
}

/// Build the whitelisted environment variables for the subprocess.
///
/// **Security:** Only these env vars are passed to the CLI process.
pub(crate) fn build_env(opts: &CliSpawnOptions) -> Vec<(String, String)> {
    let mut env = Vec::new();

    // System essentials
    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH".to_owned(), path));
    }
    if let Ok(home) = std::env::var("HOME") {
        env.push(("HOME".to_owned(), home));
    }
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        env.push(("TMPDIR".to_owned(), tmpdir));
    }

    // Auth: prefer OAuth token, fall back to API key
    if let Some(ref token) = opts.oauth_token {
        env.push(("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), token.clone()));
    } else if let Some(ref key) = opts.anthropic_api_key {
        env.push(("ANTHROPIC_API_KEY".to_owned(), key.clone()));
    }

    // Config dir
    if let Some(ref dir) = opts.config_dir {
        env.push(("CLAUDE_CONFIG_DIR".to_owned(), dir.display().to_string()));
    }

    // Extra env vars from caller
    for (key, value) in &opts.extra_env {
        env.push((key.clone(), value.clone()));
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_cli_explicit_path_exists() {
        // Use a path we know exists on all platforms
        let path = PathBuf::from("/usr/bin/env");
        if path.exists() {
            let result = find_claude_cli(Some(&path));
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), path);
        }
    }

    #[test]
    fn find_cli_explicit_path_missing() {
        let path = PathBuf::from("/nonexistent/path/to/claude");
        let result = find_claude_cli(Some(&path));
        assert!(matches!(result, Err(CliError::CliNotFound)));
    }

    #[test]
    fn spawn_options_default() {
        let opts = CliSpawnOptions::default();
        assert!(opts.cli_path.is_none());
        assert!(opts.cwd.is_none());
        assert!(opts.model.is_none());
        assert!(opts.system_prompt.is_none());
        assert!(opts.max_turns.is_none());
        assert!(opts.oauth_token.is_none());
        assert!(opts.anthropic_api_key.is_none());
        assert!(!opts.include_partial);
    }

    #[test]
    fn build_args_always_includes_stream_json() {
        let opts = CliSpawnOptions::default();
        let args = build_args(&opts);
        assert!(args.contains(&"--output-format".to_owned()));
        assert!(args.contains(&"--input-format".to_owned()));
        assert!(args.contains(&"--verbose".to_owned()));
        // No --print: initial prompt is sent via stdin, not CLI arg
        assert!(!args.contains(&"--print".to_owned()));
    }

    #[test]
    fn build_args_with_model() {
        let opts = CliSpawnOptions {
            model: Some("opus".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--model".to_owned()));
        assert!(args.contains(&"opus".to_owned()));
    }

    #[test]
    fn build_args_with_max_turns() {
        let opts = CliSpawnOptions {
            max_turns: Some(10),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--max-turns".to_owned()));
        assert!(args.contains(&"10".to_owned()));
    }

    #[test]
    fn build_args_with_resume_session() {
        let opts = CliSpawnOptions {
            resume_session: Some("session-abc".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--resume".to_owned()));
        assert!(args.contains(&"session-abc".to_owned()));
    }

    #[test]
    fn build_args_with_mcp_config() {
        let opts = CliSpawnOptions {
            mcp_config: Some(PathBuf::from("/tmp/mcp.json")),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--mcp-config".to_owned()));
        assert!(args.contains(&"/tmp/mcp.json".to_owned()));
    }

    #[test]
    fn build_args_with_permission_mode() {
        let opts = CliSpawnOptions {
            permission_mode: Some("bypassPermissions".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--permission-mode".to_owned()));
        assert!(args.contains(&"bypassPermissions".to_owned()));
    }

    #[test]
    fn build_args_include_partial() {
        let opts = CliSpawnOptions {
            include_partial: true,
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--include-partial-messages".to_owned()));
    }

    // R8: build_args with system_prompt
    #[test]
    fn build_args_with_system_prompt() {
        let opts = CliSpawnOptions {
            system_prompt: Some("You are helpful".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--system-prompt".to_owned()));
        assert!(args.contains(&"You are helpful".to_owned()));
    }

    // R8: build_args with append_system_prompt
    #[test]
    fn build_args_with_append_system_prompt() {
        let opts = CliSpawnOptions {
            append_system_prompt: Some("Extra context".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--append-system-prompt".to_owned()));
        assert!(args.contains(&"Extra context".to_owned()));
    }

    // R8: build_args with allowed_tools
    #[test]
    fn build_args_with_allowed_tools() {
        let opts = CliSpawnOptions {
            allowed_tools: Some(vec!["Read".into(), "Write".into(), "Bash".into()]),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--allowedTools".to_owned()));
        assert!(args.contains(&"Read,Write,Bash".to_owned()));
    }

    #[test]
    fn build_env_with_oauth_token() {
        let opts = CliSpawnOptions {
            oauth_token: Some("my-oauth-token".into()),
            ..Default::default()
        };
        let env = build_env(&opts);
        assert!(env
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_OAUTH_TOKEN" && v == "my-oauth-token"));
        // API key should NOT be present when oauth token is set
        assert!(env.iter().all(|(k, _)| k != "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn build_env_api_key_fallback() {
        let opts = CliSpawnOptions {
            anthropic_api_key: Some("sk-ant-key".into()),
            ..Default::default()
        };
        let env = build_env(&opts);
        assert!(env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-ant-key"));
        // OAuth token should NOT be present when only API key is set
        assert!(env.iter().all(|(k, _)| k != "CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn build_env_oauth_takes_precedence() {
        let opts = CliSpawnOptions {
            oauth_token: Some("oauth-tok".into()),
            anthropic_api_key: Some("api-key".into()),
            ..Default::default()
        };
        let env = build_env(&opts);
        assert!(env
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_OAUTH_TOKEN" && v == "oauth-tok"));
        assert!(env.iter().all(|(k, _)| k != "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn build_env_whitelist_has_path_home() {
        let opts = CliSpawnOptions::default();
        let env = build_env(&opts);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        // PATH and HOME should be present if set in the real environment
        // (can't guarantee in all test envs, but the function should include them)
        assert!(
            keys.contains(&"PATH") || std::env::var("PATH").is_err(),
            "PATH should be whitelisted"
        );
    }

    #[test]
    fn build_env_config_dir() {
        let opts = CliSpawnOptions {
            config_dir: Some(PathBuf::from("/tmp/claude-config")),
            ..Default::default()
        };
        let env = build_env(&opts);
        assert!(env
            .iter()
            .any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && v == "/tmp/claude-config"));
    }

    #[test]
    fn build_env_extra_env() {
        let opts = CliSpawnOptions {
            extra_env: vec![
                ("CUSTOM_VAR".into(), "custom_value".into()),
                ("ANOTHER".into(), "val".into()),
            ],
            ..Default::default()
        };
        let env = build_env(&opts);
        assert!(env
            .iter()
            .any(|(k, v)| k == "CUSTOM_VAR" && v == "custom_value"));
        assert!(env.iter().any(|(k, v)| k == "ANOTHER" && v == "val"));
    }

    #[test]
    fn build_env_no_database_url() {
        // The env_clear + whitelist approach means DATABASE_URL is never included
        let opts = CliSpawnOptions::default();
        let env = build_env(&opts);
        assert!(
            env.iter().all(|(k, _)| k != "DATABASE_URL"),
            "DATABASE_URL must never be passed to CLI subprocess"
        );
        assert!(
            env.iter().all(|(k, _)| k != "PLATFORM_MASTER_KEY"),
            "PLATFORM_MASTER_KEY must never be passed to CLI subprocess"
        );
    }

    /// Helper: spawn `sh -c 'exec cat'` as a mock transport.
    /// Uses shell so that CLI args are ignored — `cat` reads pure stdin.
    async fn spawn_cat_transport() -> SubprocessTransport {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exec cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn sh -c cat");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        SubprocessTransport {
            child,
            stdin: Mutex::new(BufWriter::new(stdin)),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        }
    }

    #[tokio::test]
    async fn spawn_and_kill() {
        let mut transport = spawn_cat_transport().await;
        assert!(transport.is_alive());
        transport.kill().await.unwrap();
        assert!(!transport.is_alive());
    }

    #[tokio::test]
    async fn send_and_recv_with_cat() {
        let transport = spawn_cat_transport().await;

        // Write a valid NDJSON system message — cat echoes it back
        let msg = r#"{"type":"system","subtype":"init","session_id":"test-123"}"#;
        {
            let mut stdin = transport.stdin.lock().await;
            stdin
                .write_all(format!("{msg}\n").as_bytes())
                .await
                .unwrap();
            stdin.flush().await.unwrap();
        }

        let received = transport.recv().await.unwrap();
        assert!(received.is_some());
        match received.unwrap() {
            CliMessage::System(s) => {
                assert_eq!(s.session_id, "test-123");
            }
            other => panic!("expected System, got: {other:?}"),
        }

        // Verify session_id was captured
        assert_eq!(transport.session_id().await.as_deref(), Some("test-123"));
    }

    #[tokio::test]
    async fn send_message_writes_ndjson() {
        let transport = spawn_cat_transport().await;

        // send_message writes CliUserInput JSON — cat echoes it back
        transport.send_message("hello world").await.unwrap();

        // Read raw line from stdout to verify the format
        let mut stdout = transport.stdout.lock().await;
        let mut line = String::new();
        stdout.read_line(&mut line).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["content"], "hello world");
    }

    #[tokio::test]
    async fn recv_returns_none_on_eof() {
        // Spawn a process that exits immediately after printing one line
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "echo done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = SubprocessTransport {
            child,
            stdin: Mutex::new(BufWriter::new(stdin)),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        };

        // "done" is not valid JSON — will be skipped, then EOF → None
        let result = transport.recv().await.unwrap();
        assert!(result.is_none());
        assert!(!transport.is_alive());
    }

    #[tokio::test]
    async fn recv_skips_invalid_json() {
        let transport = spawn_cat_transport().await;

        // Write invalid JSON then valid JSON
        {
            let mut stdin = transport.stdin.lock().await;
            stdin.write_all(b"not json\n").await.unwrap();
            stdin
                .write_all(br#"{"type":"system","subtype":"init","session_id":"after-invalid"}"#)
                .await
                .unwrap();
            stdin.write_all(b"\n").await.unwrap();
            stdin.flush().await.unwrap();
        }

        // Should skip the invalid line and return the valid one
        let msg = transport.recv().await.unwrap().unwrap();
        match msg {
            CliMessage::System(s) => assert_eq!(s.session_id, "after-invalid"),
            other => panic!("expected System, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_json_to_not_running_fails() {
        let mut transport = spawn_cat_transport().await;
        transport.kill().await.unwrap();

        let result = transport.send_message("hello").await;
        assert!(matches!(result, Err(CliError::NotRunning)));
    }
}
