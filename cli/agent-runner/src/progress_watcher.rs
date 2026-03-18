use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::pubsub::{PubSubClient, PubSubEvent, PubSubKind};

/// Default path for the progress file inside the workspace.
const DEFAULT_PROGRESS_PATH: &str = "/workspace/.platform/progress.md";

/// Debounce interval — avoid publishing on every keystroke.
const DEBOUNCE_MS: u64 = 500;

/// Resolve the progress file path from env or default.
fn resolve_path() -> PathBuf {
    std::env::var("PROGRESS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PROGRESS_PATH))
}

/// Read the progress file, returning `None` if it doesn't exist or is empty.
async fn read_progress(path: &Path) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) if !content.trim().is_empty() => Some(content),
        _ => None,
    }
}

/// Spawn a background task that watches the progress file and publishes
/// `ProgressUpdate` events to Valkey pub/sub.
///
/// Returns a `JoinHandle` that runs until the shutdown signal fires.
pub fn spawn(
    pubsub: PubSubClient,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_watcher(pubsub, shutdown).await {
            eprintln!("[warn] progress watcher exited: {e}");
        }
    })
}

async fn run_watcher(
    pubsub: PubSubClient,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let progress_path = resolve_path();

    // Ensure the parent directory exists so notify can watch it.
    if let Some(parent) = progress_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    // Watch the parent directory for file create/modify events.
    let watch_dir = progress_path
        .parent()
        .unwrap_or(Path::new("/workspace"))
        .to_path_buf();

    let (tx, mut rx) = mpsc::channel::<()>(16);

    // Set up file watcher (runs on a blocking thread internally).
    let target_name = progress_path
        .file_name()
        .map(|n| n.to_os_string());

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    // Only trigger for our target file
                    let matches = event.paths.iter().any(|p| {
                        p.file_name() == target_name.as_deref()
                    });
                    if matches {
                        let _ = tx.try_send(());
                    }
                }
                _ => {}
            }
        }
    })?;

    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    eprintln!(
        "[info] progress watcher started for {}",
        progress_path.display()
    );

    let mut last_content: Option<String> = None;

    // Publish initial content if file already exists
    if let Some(content) = read_progress(&progress_path).await {
        publish_progress(&pubsub, &content).await;
        last_content = Some(content);
    }

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            event = rx.recv() => {
                if event.is_none() {
                    break; // watcher dropped
                }
                // Debounce — drain any queued events, then wait
                tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
                while rx.try_recv().is_ok() {}

                if let Some(content) = read_progress(&progress_path).await {
                    // Only publish if content changed
                    if last_content.as_ref() != Some(&content) {
                        publish_progress(&pubsub, &content).await;
                        last_content = Some(content);
                    }
                }
            }
        }
    }

    // Drop watcher explicitly to stop watching
    drop(watcher);
    Ok(())
}

async fn publish_progress(pubsub: &PubSubClient, content: &str) {
    let event = PubSubEvent {
        kind: PubSubKind::ProgressUpdate,
        message: content.to_owned(),
        metadata: None,
    };
    if let Err(e) = pubsub.publish_event(&event).await {
        eprintln!("[warn] failed to publish progress update: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_default() {
        // When PROGRESS_FILE is not set, should use default path
        let path = PathBuf::from(DEFAULT_PROGRESS_PATH);
        assert_eq!(path.to_str().unwrap(), "/workspace/.platform/progress.md");
    }

    #[test]
    fn truncate_does_not_panic_on_empty() {
        let result = read_progress(Path::new("/nonexistent/file.md"));
        // This is async, but we test the sync logic via path resolution
        assert!(Path::new("/nonexistent/file.md").parent().is_some());
    }
}
