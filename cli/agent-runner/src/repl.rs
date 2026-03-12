use std::io::IsTerminal;

use anyhow::{Context, Result};

use crate::error::CliError;
use crate::messages::{CliMessage, SystemMessage};
use crate::pubsub::{cli_message_to_event, PubSubClient, PubSubInput};
use crate::render;
use crate::transport::{CliSpawnOptions, SubprocessTransport};

/// Wait for the CLI system init message within the given timeout.
pub(crate) async fn wait_for_init(
    transport: &SubprocessTransport,
    timeout_secs: u64,
) -> Result<SystemMessage, CliError> {
    let timeout = std::time::Duration::from_secs(timeout_secs);
    match tokio::time::timeout(timeout, transport.recv()).await {
        Ok(Ok(Some(CliMessage::System(sys)))) => Ok(sys),
        Ok(Ok(Some(_))) => Err(CliError::SessionError(
            "expected system init, got other message".into(),
        )),
        Ok(Ok(None)) => Err(CliError::SessionError("CLI exited before init".into())),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(CliError::InitTimeout(timeout_secs)),
    }
}

/// Publish a WaitingForInput event via pub/sub.
async fn publish_waiting(ps: &PubSubClient) {
    let event = crate::pubsub::PubSubEvent {
        kind: crate::pubsub::PubSubKind::WaitingForInput,
        message: "Agent ready — waiting for input".into(),
        metadata: None,
    };
    ps.publish_event(&event).await.ok();
}

/// Publish a Completed event via pub/sub.
async fn publish_completed(ps: &PubSubClient, message: &str) {
    let event = crate::pubsub::PubSubEvent {
        kind: crate::pubsub::PubSubKind::Completed,
        message: message.into(),
        metadata: None,
    };
    ps.publish_event(&event).await.ok();
}

/// Wait for user input from stdin or pub/sub, with signal handling.
///
/// Returns `Some(text)` when input arrives, or `None` when all sources close / signal.
async fn wait_for_input(
    stdin_rx: &mut tokio::sync::mpsc::Receiver<String>,
    pubsub_rx: &mut Option<tokio::sync::mpsc::Receiver<PubSubInput>>,
    stdin_alive: &mut bool,
    has_pubsub: bool,
    is_tty: bool,
    #[cfg(unix)] sigterm: &mut tokio::signal::unix::Signal,
) -> Option<String> {
    loop {
        if is_tty && *stdin_alive {
            eprint!("> ");
        }

        tokio::select! {
            line = async {
                if *stdin_alive { stdin_rx.recv().await } else { std::future::pending().await }
            } => {
                match line {
                    Some(text) => {
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if matches!(trimmed, "exit" | "/exit" | "quit" | "/quit") {
                            return None;
                        }
                        return Some(text);
                    }
                    None => {
                        *stdin_alive = false;
                        if has_pubsub {
                            continue;
                        }
                        return None;
                    }
                }
            }
            input = async {
                match pubsub_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match input {
                    Some(PubSubInput::Prompt { content, .. }) => {
                        let trimmed = content.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        return Some(content);
                    }
                    Some(PubSubInput::Control { .. }) => {
                        // Control messages (interrupt) are only meaningful during
                        // an active turn — ignore while waiting for input.
                        continue;
                    }
                    None => return None,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[info] Ctrl-C, exiting...");
                return None;
            }
            _ = async {
                #[cfg(unix)]
                sigterm.recv().await;
                #[cfg(not(unix))]
                std::future::pending::<()>().await;
            } => {
                eprintln!("[info] SIGTERM received, shutting down...");
                return None;
            }
        }
    }
}

/// Stream responses from CLI until Result or EOF.
///
/// Returns `true` if the turn completed normally (non-error Result),
/// `false` if the session should end (EOF, error result, read error).
async fn stream_turn_responses(
    transport: &SubprocessTransport,
    pubsub: &Option<PubSubClient>,
) -> Result<bool> {
    loop {
        let msg = transport.recv().await;
        match msg {
            Ok(Some(ref m)) => {
                render::render_message(m);

                if let Some(ref ps) = pubsub {
                    if let Some(event) = cli_message_to_event(m) {
                        ps.publish_event(&event).await.ok();
                    }
                }

                if let CliMessage::Result(ref r) = m {
                    render::notify_desktop(
                        "agent-runner",
                        if r.is_error {
                            "Agent completed with error"
                        } else {
                            "Agent turn completed"
                        },
                    );
                    if r.is_error {
                        return Ok(false);
                    }
                    return Ok(true); // Turn done, continue to next input
                }
            }
            Ok(None) => {
                // CLI exited (EOF) — expected in -p mode after Result
                return Ok(false);
            }
            Err(e) => {
                eprintln!("[error] CLI read error: {e}");
                return Ok(false);
            }
        }
    }
}

/// Run the per-turn spawn REPL + pub/sub bridge event loop.
///
/// Each user message spawns a fresh CLI process with `-p` and `--resume`.
/// Between turns, publishes `WaitingForInput` and waits for stdin/pub-sub input.
pub async fn run(
    base_opts: CliSpawnOptions,
    pubsub: Option<PubSubClient>,
    initial_prompt: Option<String>,
    mut stdin_rx: tokio::sync::mpsc::Receiver<String>,
) -> Result<()> {
    // Register SIGTERM handler for K8s graceful shutdown
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to register SIGTERM handler")?;

    let is_tty = std::io::stdin().is_terminal();
    let has_pubsub = pubsub.is_some();

    // Subscribe to pubsub input once (persists across turns)
    let mut pubsub_rx = if let Some(ref ps) = pubsub {
        Some(ps.subscribe_input().await?)
    } else {
        None
    };

    let mut cli_session_id: Option<String> = None;
    let mut stdin_alive = true;
    // Sessions started with an explicit prompt are "fire-and-forget" — they exit
    // after the first successful turn (e.g. create-app worker pods).
    // Sessions started idle (no prompt) stay alive for interactive follow-ups
    // (e.g. agent chat panel).
    let started_with_prompt = initial_prompt.is_some();
    let mut pending_prompt = initial_prompt;

    // If no initial prompt and pubsub active, publish WaitingForInput immediately
    if pending_prompt.is_none() {
        if let Some(ref ps) = pubsub {
            publish_waiting(ps).await;
        }
    }

    loop {
        // Phase A: Get user input (or use pending prompt)
        let user_message = if let Some(prompt) = pending_prompt.take() {
            prompt
        } else {
            // Publish WaitingForInput between turns
            if let Some(ref ps) = pubsub {
                publish_waiting(ps).await;
            }

            match wait_for_input(
                &mut stdin_rx,
                &mut pubsub_rx,
                &mut stdin_alive,
                has_pubsub,
                is_tty,
                #[cfg(unix)]
                &mut sigterm,
            )
            .await
            {
                Some(text) => text,
                None => {
                    // Signal or all input sources closed — publish error and exit
                    if let Some(ref ps) = pubsub {
                        let event = crate::pubsub::PubSubEvent {
                            kind: crate::pubsub::PubSubKind::Error,
                            message: "Session terminated".into(),
                            metadata: None,
                        };
                        ps.publish_event(&event).await.ok();
                    }
                    break;
                }
            }
        };

        // Phase B: Spawn CLI for this turn
        let mut turn_opts = base_opts.clone();
        turn_opts.prompt = Some(user_message);
        if let Some(ref sid) = cli_session_id {
            turn_opts.resume_session = Some(sid.clone());
            turn_opts.initial_session_id = None; // Don't set --session-id on resume
        }

        let transport = match SubprocessTransport::spawn(turn_opts) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[error] failed to spawn CLI: {e}");
                if let Some(ref ps) = pubsub {
                    let event = crate::pubsub::PubSubEvent {
                        kind: crate::pubsub::PubSubKind::Error,
                        message: format!("Failed to spawn CLI: {e}"),
                        metadata: None,
                    };
                    ps.publish_event(&event).await.ok();
                }
                break;
            }
        };

        // Close stdin — -p mode doesn't use it
        transport.close_stdin().await;

        // Phase C: Read system init
        let sys = match wait_for_init(&transport, 600).await {
            Ok(sys) => sys,
            Err(e) => {
                eprintln!("[error] CLI init failed: {e}");
                if let Some(ref ps) = pubsub {
                    let event = crate::pubsub::PubSubEvent {
                        kind: crate::pubsub::PubSubKind::Error,
                        message: format!("CLI init failed: {e}"),
                        metadata: None,
                    };
                    ps.publish_event(&event).await.ok();
                }
                break;
            }
        };

        render::render_message(&CliMessage::System(sys.clone()));

        // Capture session ID for --resume on subsequent turns
        cli_session_id = Some(sys.session_id.clone());

        // Publish init event
        if let Some(ref ps) = pubsub {
            if let Some(event) = cli_message_to_event(&CliMessage::System(sys)) {
                if let Err(e) = ps.publish_event(&event).await {
                    eprintln!("[warn] failed to publish init event: {e}");
                }
            }
        }

        // Phase D: Stream responses until Result or EOF
        let should_continue = stream_turn_responses(&transport, &pubsub).await?;
        // transport dropped here — CLI process cleaned up

        if !should_continue {
            if let Some(ref ps) = pubsub {
                publish_completed(ps, "Agent session ended").await;
            }
            break;
        }

        // Fire-and-forget mode: if started with an explicit prompt, exit after
        // the first successful turn. The pod will reach Succeeded phase.
        // Interactive sessions (started idle) stay alive for follow-up messages.
        if started_with_prompt {
            if let Some(ref ps) = pubsub {
                publish_completed(ps, "Agent completed").await;
            }
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;
    use tokio::io::{BufReader, BufWriter};
    use tokio::sync::Mutex;

    use crate::pubsub::{dispatch_input, PubSubInput};

    /// Spawn `sh -c 'exec cat'` as a mock transport.
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
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Create a temp script that acts as a mock CLI.
    /// The script ignores all args and emits the given NDJSON lines.
    fn mock_cli_script(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        for line in lines {
            writeln!(f, "echo '{line}'").unwrap();
        }
        f.flush().unwrap();

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(f.path()).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(f.path(), perms).unwrap();
        }

        f
    }

    #[tokio::test]
    async fn init_timeout_triggers() {
        let transport = spawn_cat_transport().await;
        let result = wait_for_init(&transport, 1).await;
        assert!(matches!(result, Err(CliError::InitTimeout(1))));
    }

    #[tokio::test]
    async fn init_succeeds_with_system_message() {
        let transport = spawn_cat_transport().await;

        let msg = r#"{"type":"system","subtype":"init","session_id":"test-init","model":"opus"}"#;
        {
            let mut guard = transport.stdin.lock().await;
            let stdin = guard.as_mut().unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut *stdin, format!("{msg}\n").as_bytes())
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::flush(&mut *stdin).await.unwrap();
        }

        let sys = wait_for_init(&transport, 5).await.unwrap();
        assert_eq!(sys.session_id, "test-init");
        assert_eq!(sys.model.as_deref(), Some("opus"));
    }

    #[tokio::test]
    async fn init_eof_before_system_message() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "true"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = SubprocessTransport {
            child,
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        };

        let result = wait_for_init(&transport, 5).await;
        assert!(matches!(result, Err(CliError::SessionError(_))));
        if let Err(CliError::SessionError(msg)) = result {
            assert!(msg.contains("exited before init"));
        }
    }

    #[tokio::test]
    async fn init_wrong_message_type() {
        let transport = spawn_cat_transport().await;

        let msg = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        {
            let mut guard = transport.stdin.lock().await;
            let stdin = guard.as_mut().unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut *stdin, format!("{msg}\n").as_bytes())
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::flush(&mut *stdin).await.unwrap();
        }

        let result = wait_for_init(&transport, 5).await;
        assert!(matches!(result, Err(CliError::SessionError(_))));
        if let Err(CliError::SessionError(msg)) = result {
            assert!(msg.contains("expected system init"));
        }
    }

    #[tokio::test]
    async fn dispatch_prompt_input() {
        let transport = spawn_cat_transport().await;
        let input = PubSubInput::Prompt {
            content: "hello".into(),
            source: None,
        };
        dispatch_input(&transport, input).await.unwrap();

        let mut stdout = transport.stdout.lock().await;
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut *stdout, &mut line)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["content"], "hello");
    }

    #[tokio::test]
    async fn dispatch_control_interrupt() {
        let transport = spawn_cat_transport().await;
        let input = PubSubInput::Control {
            control: crate::control::ControlPayload::Interrupt,
        };
        dispatch_input(&transport, input).await.unwrap();

        let mut stdout = transport.stdout.lock().await;
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut *stdout, &mut line)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "control");
        assert_eq!(parsed["control"]["type"], "interrupt");
    }

    #[tokio::test]
    async fn loop_exits_on_result_message() {
        let mut child = tokio::process::Command::new("sh")
            .args([
                "-c",
                r#"echo '{"type":"system","subtype":"init","session_id":"s1"}'; echo '{"type":"result","subtype":"success","session_id":"s1","is_error":false}';"#,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = SubprocessTransport {
            child,
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        };

        let sys = wait_for_init(&transport, 5).await.unwrap();
        assert_eq!(sys.session_id, "s1");

        let msg = transport.recv().await.unwrap();
        assert!(matches!(msg, Some(CliMessage::Result(_))));
    }

    #[tokio::test]
    async fn loop_exits_on_eof() {
        let mut child = tokio::process::Command::new("sh")
            .args([
                "-c",
                r#"echo '{"type":"system","subtype":"init","session_id":"s2"}'"#,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = SubprocessTransport {
            child,
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        };

        let _sys = wait_for_init(&transport, 5).await.unwrap();

        let msg = transport.recv().await.unwrap();
        assert!(msg.is_none());
    }

    /// Test the full `run()` with a mock CLI script that emits init + result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_completes_with_init_and_result() {
        let script = mock_cli_script(&[
            r#"{"type":"system","subtype":"init","session_id":"run-test","model":"test"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"run-test","is_error":false,"result":"done"}"#,
        ]);
        let opts = CliSpawnOptions {
            cli_path: Some(script.path().to_path_buf()),
            ..Default::default()
        };

        let (tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(1);
        drop(tx); // Close stdin so run() exits after first turn
        let result = run(opts, None, Some("Hello agent".into()), stdin_rx).await;
        assert!(result.is_ok());
    }

    /// Test run() exits cleanly when the process exits after init (EOF path).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_exits_on_eof_after_init() {
        let script = mock_cli_script(&[
            r#"{"type":"system","subtype":"init","session_id":"eof-test"}"#,
        ]);
        let opts = CliSpawnOptions {
            cli_path: Some(script.path().to_path_buf()),
            ..Default::default()
        };

        let (_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(1);
        let result = run(opts, None, Some("test".into()), stdin_rx).await;
        assert!(result.is_ok());
    }

    /// Test run() when CLI exits before sending system init — exits gracefully.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_exits_gracefully_when_no_init() {
        let script = mock_cli_script(&[]); // exits immediately, no output
        let opts = CliSpawnOptions {
            cli_path: Some(script.path().to_path_buf()),
            ..Default::default()
        };

        let (_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(1);
        let result = run(opts, None, Some("test".into()), stdin_rx).await;
        // run() handles init failure gracefully (logs error, breaks loop)
        assert!(result.is_ok());
    }

    /// Test run() with error result from CLI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_handles_error_result() {
        let script = mock_cli_script(&[
            r#"{"type":"system","subtype":"init","session_id":"err-test"}"#,
            r#"{"type":"result","subtype":"error","session_id":"err-test","is_error":true,"result":"Rate limit"}"#,
        ]);
        let opts = CliSpawnOptions {
            cli_path: Some(script.path().to_path_buf()),
            ..Default::default()
        };

        let (_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(1);
        let result = run(opts, None, Some("test".into()), stdin_rx).await;
        assert!(result.is_ok());
    }

    /// Test stream_turn_responses returns true on non-error Result.
    #[tokio::test]
    async fn stream_turn_responses_returns_true_on_success() {
        let mut child = tokio::process::Command::new("sh")
            .args([
                "-c",
                r#"echo '{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}'; echo '{"type":"result","subtype":"success","session_id":"s","is_error":false}';"#,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = SubprocessTransport {
            child,
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        };

        let result = stream_turn_responses(&transport, &None).await.unwrap();
        assert!(result);
    }

    /// Test stream_turn_responses returns false on error Result.
    #[tokio::test]
    async fn stream_turn_responses_returns_false_on_error() {
        let mut child = tokio::process::Command::new("sh")
            .args([
                "-c",
                r#"echo '{"type":"result","subtype":"error","session_id":"s","is_error":true,"result":"fail"}';"#,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = SubprocessTransport {
            child,
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            stdout: Mutex::new(BufReader::new(stdout)),
            stderr_task: None,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
        };

        let result = stream_turn_responses(&transport, &None).await.unwrap();
        assert!(!result);
    }
}
