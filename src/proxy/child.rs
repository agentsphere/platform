//! Child process management: spawn, signal forwarding, zombie reaping.

use std::process::Stdio;

use tokio::io::BufReader;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::watch;

/// The result of spawning a child process.
pub struct SpawnedChild {
    pub child: Child,
    pub stdout: BufReader<ChildStdout>,
    pub stderr: BufReader<ChildStderr>,
}

/// Spawn the child process and capture stdout/stderr.
#[tracing::instrument(skip_all, fields(command = %command, args_count = args.len()))]
pub fn spawn(command: &str, args: &[String]) -> anyhow::Result<SpawnedChild> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn child process '{command}': {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stderr"))?;

    tracing::info!(
        pid = child.id().unwrap_or(0),
        command,
        "child process spawned"
    );

    Ok(SpawnedChild {
        child,
        stdout: BufReader::new(stdout),
        stderr: BufReader::new(stderr),
    })
}

/// Send a signal to a child process via kill(2).
#[cfg(unix)]
pub fn signal_child(child: &Child, sig: nix::sys::signal::Signal) -> anyhow::Result<()> {
    if let Some(pid) = child.id() {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(0)),
            sig,
        )
        .map_err(|e| anyhow::anyhow!("signal failed: {e}"))?;
    }
    Ok(())
}

/// Reap zombie child processes. We are PID 1 in the container, so we must
/// periodically call `waitpid(-1, WNOHANG)` to clean up any orphaned children.
#[cfg(unix)]
#[tracing::instrument(skip_all)]
pub async fn reap_zombies(mut shutdown: watch::Receiver<()>) {
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::Pid;

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                loop {
                    match waitpid(Pid::from_raw(-1), Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::StillAlive) | Err(_) => break,
                        Ok(status) => {
                            tracing::debug!(status = ?status, "reaped zombie process");
                        }
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
}

/// Reap zombies stub for non-unix platforms.
#[cfg(not(unix))]
pub async fn reap_zombies(mut shutdown: watch::Receiver<()>) {
    let _ = shutdown.changed().await;
}

/// Wait for the child to exit, returning its exit code.
/// Signals shutdown to all other tasks when the child exits.
#[tracing::instrument(skip_all)]
pub async fn wait_for_exit(child: &mut Child, shutdown_tx: watch::Sender<()>) -> i32 {
    let exit_status = child.wait().await;
    // Signal shutdown to all other tasks
    let _ = shutdown_tx.send(());

    match exit_status {
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            tracing::info!(exit_code = code, "child process exited");
            code
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to wait for child process");
            1
        }
    }
}
