use std::time::Duration;

use tokio::sync::watch;

use crate::store::AppState;

/// Background garbage collection task for the registry.
/// Runs hourly to clean up:
/// - Orphaned blobs (no `blob_links`, older than 24h grace period)
/// - Expired upload temp files in `MinIO`
pub async fn run(state: AppState, mut shutdown: watch::Receiver<()>) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    state.task_registry.register("registry_gc", 7200);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("registry GC shutting down");
                break;
            }
            _ = interval.tick() => {
                match collect_garbage(&state).await {
                    Ok(()) => state.task_registry.heartbeat("registry_gc"),
                    Err(e) => {
                        state.task_registry.report_error("registry_gc", &e.to_string());
                        tracing::error!(error = %e, "registry GC failed");
                    }
                }
            }
        }
    }
}

pub async fn collect_garbage(state: &AppState) -> anyhow::Result<()> {
    // Find orphaned blobs: no blob_links, created more than 24h ago (grace period)
    let orphans = sqlx::query!(
        r#"SELECT digest, minio_path
           FROM registry_blobs b
           WHERE NOT EXISTS (
               SELECT 1 FROM registry_blob_links bl WHERE bl.blob_digest = b.digest
           )
           AND b.created_at < now() - interval '24 hours'"#,
    )
    .fetch_all(&state.pool)
    .await?;

    if !orphans.is_empty() {
        tracing::info!(
            count = orphans.len(),
            "registry GC: cleaning orphaned blobs"
        );
    }

    for orphan in &orphans {
        // Delete from MinIO first, then from DB
        if let Err(e) = state.minio.delete(&orphan.minio_path).await {
            tracing::warn!(error = %e, digest = %orphan.digest, "registry GC: failed to delete blob from storage");
            continue; // Skip DB deletion so we retry next cycle
        }

        if let Err(e) = sqlx::query!(
            "DELETE FROM registry_blobs WHERE digest = $1",
            orphan.digest,
        )
        .execute(&state.pool)
        .await
        {
            tracing::warn!(error = %e, digest = %orphan.digest, "registry GC: failed to delete blob from DB");
        }
    }

    if !orphans.is_empty() {
        tracing::info!(
            deleted = orphans.len(),
            "registry GC: orphaned blobs cleaned"
        );
    }

    Ok(())
}
