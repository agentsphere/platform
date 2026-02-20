# Plan 20 — Preview Environments

## Overview

Auto-create short-lived preview deployments when pipelines succeed on non-main branches. Feature branches get an isolated preview URL without manual deployment configuration. Previews are automatically cleaned up when the associated merge request is merged or when a configurable TTL expires.

**This corresponds to Agent DX Phase D from Plan 14.**

---

## Motivation

- **Table-stakes for modern dev workflows**: Feature branches need a way to show running previews for review
- **Essential for agent-driven development**: Agents pushing to feature branches should be able to demonstrate working code in a live environment
- **Reduces deployment friction**: No manual deployment configuration needed — push triggers preview automatically
- **Reviewer productivity**: MR reviewers can click a preview link instead of checking out the branch locally

---

## Prerequisites

| Requirement | Status |
|---|---|
| Pipeline executor (`src/pipeline/executor.rs`) | Complete — detects images from successful builds |
| Deployer reconciler (`src/deployer/reconciler.rs`) | Complete — pattern to replicate for previews |
| Deployment API (`src/api/deployments.rs`) | Complete — model for preview API |
| K8s client in AppState | Complete |
| MR merge handler (`src/api/merge_requests.rs`) | Complete — hook point for cleanup |

---

## Architecture

### Lifecycle

```
git push feature-branch
    → pipeline triggered
    → pipeline succeeds with container image
    → preview_deployments upserted (branch_slug, image_ref)
    → preview reconciler applies K8s manifests
    → preview URL: preview-{branch_slug}.{project}.{base_domain}

MR merged (or TTL expires)
    → preview_deployments.desired_status = 'stopped'
    → reconciler scales to 0
    → cleanup task deletes K8s resources + DB row
```

### K8s Resource Naming

Each preview gets:
- **Namespace**: `preview-{project_slug}-{branch_slug}` (max 63 chars, K8s DNS limit)
- **Deployment**: `preview` (one per namespace)
- **Service**: `preview` (one per namespace)
- **Ingress** (optional): `preview-{branch_slug}.{project}.{base_domain}`

### Branch Slug Generation

Branch names need to be converted to K8s-safe names:
- Replace `/`, `.`, `_`, `#` with `-`
- Lowercase everything
- Strip leading/trailing `-`
- Truncate to 63 chars (K8s DNS label limit)
- Handle collisions by appending short hash if needed

---

## Detailed Implementation

### Step D1: Database Migration

**New: `migrations/YYYYMMDDHHMMSS_preview_deployments.up.sql`**

```sql
CREATE TABLE preview_deployments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch          TEXT NOT NULL,
    branch_slug     TEXT NOT NULL,
    image_ref       TEXT NOT NULL,
    pipeline_id     UUID REFERENCES pipelines(id),
    desired_status  TEXT NOT NULL DEFAULT 'active'
        CHECK (desired_status IN ('active', 'stopped')),
    current_status  TEXT NOT NULL DEFAULT 'pending'
        CHECK (current_status IN ('pending', 'syncing', 'healthy', 'degraded', 'failed', 'stopped')),
    ttl_hours       INT NOT NULL DEFAULT 24,
    expires_at      TIMESTAMPTZ NOT NULL DEFAULT now() + INTERVAL '24 hours',
    created_by      UUID REFERENCES users(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, branch_slug)
);

CREATE INDEX idx_preview_deployments_status
    ON preview_deployments(current_status)
    WHERE desired_status = 'active';

CREATE INDEX idx_preview_deployments_expires
    ON preview_deployments(expires_at)
    WHERE desired_status = 'active';
```

**New: `migrations/YYYYMMDDHHMMSS_preview_deployments.down.sql`**

```sql
DROP TABLE IF EXISTS preview_deployments;
```

### Step D2: Branch Slug Helper

**Add to: `src/pipeline/mod.rs`** (or a new `src/utils.rs`)

```rust
/// Convert a git branch name to a K8s-safe DNS label.
///
/// Rules:
/// - Lowercase all characters
/// - Replace /, ., _, # with -
/// - Collapse multiple consecutive - into one
/// - Strip leading/trailing -
/// - Truncate to 63 characters (K8s DNS label limit)
/// - If empty after processing, return "preview"
pub fn slugify_branch(branch: &str) -> String {
    let slug: String = branch
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            '/' | '.' | '_' | '#' | ' ' => '-',
            c if c.is_ascii_alphanumeric() || c == '-' => c,
            _ => '-',
        })
        .collect();

    // Collapse multiple dashes
    let mut result = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash {
                result.push(c);
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    // Strip leading/trailing dashes, truncate
    let trimmed = result.trim_matches('-');
    let truncated = if trimmed.len() > 63 {
        // Truncate at 63, but don't end on a dash
        trimmed[..63].trim_end_matches('-')
    } else {
        trimmed
    };

    if truncated.is_empty() {
        "preview".to_string()
    } else {
        truncated.to_string()
    }
}
```

**Unit tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_simple_branch() {
        assert_eq!(slugify_branch("feature/add-login"), "feature-add-login");
    }

    #[test]
    fn slugify_complex_branch() {
        assert_eq!(slugify_branch("feature/CAPS_and.dots#hash"), "feature-caps-and-dots-hash");
    }

    #[test]
    fn slugify_collapses_dashes() {
        assert_eq!(slugify_branch("a//b..c__d"), "a-b-c-d");
    }

    #[test]
    fn slugify_strips_edges() {
        assert_eq!(slugify_branch("/leading/"), "leading");
        assert_eq!(slugify_branch("---"), "preview");
    }

    #[test]
    fn slugify_truncates_to_63() {
        let long = "a".repeat(100);
        assert!(slugify_branch(&long).len() <= 63);
    }

    #[test]
    fn slugify_handles_empty() {
        assert_eq!(slugify_branch(""), "preview");
    }

    #[test]
    fn slugify_preserves_numbers() {
        assert_eq!(slugify_branch("release/v1.2.3"), "release-v1-2-3");
    }
}
```

---

### Step D3: Extend Pipeline Executor

**Modify: `src/pipeline/executor.rs`** — in `detect_and_write_deployment()`:

Currently, this function only creates production deployments for main/master branch pushes. Extend it to create preview deployments for non-main branches.

```rust
async fn detect_and_write_deployment(
    state: &AppState,
    pipeline: &Pipeline,
    steps: &[PipelineStep],
) -> Result<(), anyhow::Error> {
    // Find successful step with image output
    let image_ref = find_built_image(steps);
    let Some(image_ref) = image_ref else { return Ok(()); };

    // Extract branch from git_ref
    let branch = pipeline.git_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&pipeline.git_ref);

    let is_main = matches!(branch, "main" | "master");

    if is_main {
        // Existing production deployment logic (unchanged)
        upsert_production_deployment(state, pipeline, &image_ref).await?;
    } else {
        // NEW: Create/update preview deployment
        upsert_preview_deployment(state, pipeline, branch, &image_ref).await?;
    }

    Ok(())
}

async fn upsert_preview_deployment(
    state: &AppState,
    pipeline: &Pipeline,
    branch: &str,
    image_ref: &str,
) -> Result<(), anyhow::Error> {
    let slug = crate::pipeline::slugify_branch(branch);

    sqlx::query!(
        r#"INSERT INTO preview_deployments
            (project_id, branch, branch_slug, image_ref, pipeline_id, created_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (project_id, branch_slug) DO UPDATE SET
            image_ref = EXCLUDED.image_ref,
            pipeline_id = EXCLUDED.pipeline_id,
            desired_status = 'active',
            current_status = 'pending',
            expires_at = now() + (preview_deployments.ttl_hours || ' hours')::interval,
            updated_at = now()"#,
        pipeline.project_id,
        branch,
        slug,
        image_ref,
        pipeline.id,
        pipeline.triggered_by,
    )
    .execute(&state.pool)
    .await?;

    tracing::info!(
        project_id = %pipeline.project_id,
        branch = %branch,
        slug = %slug,
        image = %image_ref,
        "preview deployment upserted"
    );

    // Fire webhook for preview event
    crate::api::webhooks::fire_webhooks(
        &state.pool,
        pipeline.project_id,
        "deploy",
        &serde_json::json!({
            "action": "preview_created",
            "branch": branch,
            "branch_slug": slug,
            "image_ref": image_ref,
            "pipeline_id": pipeline.id,
        }),
    ).await;

    Ok(())
}
```

---

### Step D4: Preview Reconciler

**New: `src/deployer/preview.rs`** (~200 lines)

Background task that runs every 15 seconds, reconciling pending preview deployments and cleaning up expired ones.

```rust
use crate::store::AppState;
use tokio::sync::watch;
use uuid::Uuid;

/// Background task: reconcile preview deployments every 15 seconds.
pub async fn run(state: AppState, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = reconcile(&state).await {
                    tracing::error!(error = %e, "preview reconciliation failed");
                }
                if let Err(e) = cleanup_expired(&state).await {
                    tracing::error!(error = %e, "preview cleanup failed");
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("preview reconciler shutting down");
                break;
            }
        }
    }
}
```

#### Reconcile Pending Previews

```rust
struct PendingPreview {
    id: Uuid,
    project_id: Uuid,
    branch_slug: String,
    image_ref: String,
    project_name: String,
}

async fn reconcile(state: &AppState) -> Result<(), anyhow::Error> {
    // Find previews needing reconciliation
    let pending = sqlx::query_as!(
        PendingPreview,
        r#"SELECT pd.id, pd.project_id, pd.branch_slug, pd.image_ref,
                  p.name as project_name
           FROM preview_deployments pd
           JOIN projects p ON p.id = pd.project_id
           WHERE pd.desired_status = 'active'
             AND pd.current_status IN ('pending', 'syncing')
           LIMIT 5"#,
    )
    .fetch_all(&state.pool)
    .await?;

    for preview in pending {
        tokio::spawn(reconcile_one(state.clone(), preview));
    }

    Ok(())
}

async fn reconcile_one(state: AppState, preview: PendingPreview) {
    // Mark as syncing
    let _ = sqlx::query!(
        "UPDATE preview_deployments SET current_status = 'syncing', updated_at = now() WHERE id = $1",
        preview.id,
    )
    .execute(&state.pool)
    .await;

    match apply_preview_manifests(&state, &preview).await {
        Ok(()) => {
            let _ = sqlx::query!(
                "UPDATE preview_deployments SET current_status = 'healthy', updated_at = now() WHERE id = $1",
                preview.id,
            )
            .execute(&state.pool)
            .await;
            tracing::info!(
                preview_id = %preview.id,
                slug = %preview.branch_slug,
                "preview deployed successfully"
            );
        }
        Err(e) => {
            let _ = sqlx::query!(
                "UPDATE preview_deployments SET current_status = 'failed', updated_at = now() WHERE id = $1",
                preview.id,
            )
            .execute(&state.pool)
            .await;
            tracing::error!(
                preview_id = %preview.id,
                error = %e,
                "preview deployment failed"
            );
        }
    }
}
```

#### Apply K8s Manifests

```rust
async fn apply_preview_manifests(
    state: &AppState,
    preview: &PendingPreview,
) -> Result<(), anyhow::Error> {
    let ns_name = format!(
        "preview-{}-{}",
        crate::pipeline::slug(&preview.project_name),
        &preview.branch_slug
    );

    // Ensure namespace exists
    ensure_namespace(&state.kube, &ns_name).await?;

    // Create/update Deployment
    let deployment = build_preview_deployment(preview, &ns_name);
    apply_deployment(&state.kube, &ns_name, deployment).await?;

    // Create/update Service
    let service = build_preview_service(preview, &ns_name);
    apply_service(&state.kube, &ns_name, service).await?;

    Ok(())
}

fn build_preview_deployment(preview: &PendingPreview, namespace: &str) -> k8s_openapi::api::apps::v1::Deployment {
    // Single replica, port 8080, with preview labels
    // Labels: platform.io/component=preview, platform.io/project={project_id},
    //         platform.io/branch-slug={branch_slug}
    // Container: preview.image_ref, 8080 port, resource limits (100m/128Mi request, 500m/512Mi limit)
    todo!("construct K8s Deployment object")
}

fn build_preview_service(preview: &PendingPreview, namespace: &str) -> k8s_openapi::api::core::v1::Service {
    // ClusterIP service targeting port 8080
    todo!("construct K8s Service object")
}
```

#### Cleanup Expired Previews

```rust
async fn cleanup_expired(state: &AppState) -> Result<(), anyhow::Error> {
    let expired = sqlx::query!(
        r#"SELECT id, project_id, branch_slug
           FROM preview_deployments
           WHERE desired_status = 'active'
             AND expires_at < now()"#,
    )
    .fetch_all(&state.pool)
    .await?;

    for row in expired {
        tracing::info!(
            preview_id = %row.id,
            slug = %row.branch_slug,
            "cleaning up expired preview"
        );

        // Mark as stopped
        let _ = sqlx::query!(
            "UPDATE preview_deployments SET desired_status = 'stopped', current_status = 'stopped', updated_at = now() WHERE id = $1",
            row.id,
        )
        .execute(&state.pool)
        .await;

        // Delete K8s namespace (cascading delete cleans up all resources)
        let project_name = sqlx::query_scalar!(
            "SELECT name FROM projects WHERE id = $1",
            row.project_id,
        )
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or_default();

        let ns_name = format!(
            "preview-{}-{}",
            crate::pipeline::slug(&project_name),
            &row.branch_slug
        );

        if let Err(e) = delete_namespace(&state.kube, &ns_name).await {
            tracing::warn!(error = %e, namespace = %ns_name, "failed to delete preview namespace");
        }
    }

    Ok(())
}
```

---

### Step D5: Cleanup on MR Merge

**Modify: `src/api/merge_requests.rs`** — after successful merge:

```rust
async fn merge_merge_request(/* ... */) -> Result<Json<MergeRequestResponse>, ApiError> {
    // ... existing merge logic ...

    // After successful merge, stop the preview deployment for this branch
    let source_branch = &mr.source_branch;
    let slug = crate::pipeline::slugify_branch(source_branch);

    sqlx::query!(
        r#"UPDATE preview_deployments
           SET desired_status = 'stopped', updated_at = now()
           WHERE project_id = $1 AND branch_slug = $2 AND desired_status = 'active'"#,
        project_id,
        slug,
    )
    .execute(&state.pool)
    .await
    .ok(); // Don't fail the merge if preview cleanup fails

    tracing::info!(
        project_id = %project_id,
        branch = %source_branch,
        "preview deployment stopped on MR merge"
    );

    // ... return response ...
}
```

---

### Step D6: Preview API Endpoints

**Modify: `src/api/deployments.rs`** — add preview endpoints:

```rust
// Add to the deployments router
pub fn router() -> Router<AppState> {
    Router::new()
        // ... existing deployment routes ...
        .route("/api/projects/{project_id}/previews", get(list_previews))
        .route("/api/projects/{project_id}/previews/{slug}", get(get_preview).delete(delete_preview))
}
```

#### List Previews

```rust
#[derive(Debug, serde::Serialize)]
pub struct PreviewResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub branch: String,
    pub branch_slug: String,
    pub image_ref: String,
    pub pipeline_id: Option<Uuid>,
    pub desired_status: String,
    pub current_status: String,
    pub ttl_hours: i32,
    pub expires_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

async fn list_previews(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(project_id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<PreviewResponse>>, ApiError> {
    require_project_read(&state, &auth, project_id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let items = sqlx::query_as!(
        PreviewResponse,
        r#"SELECT id, project_id, branch, branch_slug, image_ref, pipeline_id,
                  desired_status, current_status, ttl_hours, expires_at,
                  created_by, created_at, updated_at
           FROM preview_deployments
           WHERE project_id = $1 AND desired_status = 'active'
           ORDER BY created_at DESC
           LIMIT $2 OFFSET $3"#,
        project_id, limit, offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let total = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM preview_deployments WHERE project_id = $1 AND desired_status = 'active'",
        project_id,
    )
    .fetch_one(&state.pool)
    .await?
    .unwrap_or(0);

    Ok(Json(ListResponse { items, total }))
}
```

#### Get Preview

```rust
async fn get_preview(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((project_id, slug)): Path<(Uuid, String)>,
) -> Result<Json<PreviewResponse>, ApiError> {
    require_project_read(&state, &auth, project_id).await?;

    let preview = sqlx::query_as!(
        PreviewResponse,
        r#"SELECT id, project_id, branch, branch_slug, image_ref, pipeline_id,
                  desired_status, current_status, ttl_hours, expires_at,
                  created_by, created_at, updated_at
           FROM preview_deployments
           WHERE project_id = $1 AND branch_slug = $2 AND desired_status = 'active'"#,
        project_id, slug,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("preview".into()))?;

    Ok(Json(preview))
}
```

#### Delete Preview

```rust
async fn delete_preview(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((project_id, slug)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_write(&state, &auth, project_id).await?;

    let result = sqlx::query!(
        r#"UPDATE preview_deployments
           SET desired_status = 'stopped', updated_at = now()
           WHERE project_id = $1 AND branch_slug = $2 AND desired_status = 'active'"#,
        project_id, slug,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("preview".into()));
    }

    // Audit log
    crate::audit::write_audit(&state.pool, crate::audit::AuditEntry {
        actor_id: auth.user_id,
        actor_name: &auth.user_name,
        action: "preview.delete",
        resource: "preview_deployment",
        resource_id: None,
        project_id: Some(project_id),
        detail: Some(serde_json::json!({"branch_slug": slug})),
        ip_addr: auth.ip_addr.as_deref(),
    }).await;

    Ok(Json(serde_json::json!({"ok": true})))
}
```

---

### Step D7: Wire Preview Reconciler into Main

**Modify: `src/main.rs`** — spawn preview reconciler background task:

```rust
// In the background tasks section, alongside deployer::reconciler::run()
tokio::spawn(crate::deployer::preview::run(state.clone(), shutdown_rx.clone()));
```

**Modify: `src/deployer/mod.rs`** — add `pub mod preview;`

---

## Deploy MCP Server Integration

The `platform-deploy` MCP server (Plan 18) already includes `list_previews` and `get_preview` tools. After this plan, those tools will have data to return.

---

## Files Changed

| File | Action | Description |
|------|--------|-------------|
| `migrations/YYYYMMDDHHMMSS_preview_deployments.up.sql` | **New** | preview_deployments table |
| `migrations/YYYYMMDDHHMMSS_preview_deployments.down.sql` | **New** | Drop table |
| `src/pipeline/mod.rs` | **Modify** | Add `slugify_branch()` |
| `src/pipeline/executor.rs` | **Modify** | Extend `detect_and_write_deployment()` for non-main branches |
| `src/deployer/preview.rs` | **New** | Preview reconciler background task (~200 lines) |
| `src/deployer/mod.rs` | **Modify** | Add `pub mod preview;` |
| `src/api/deployments.rs` | **Modify** | Add preview API endpoints (list, get, delete) |
| `src/api/merge_requests.rs` | **Modify** | Stop preview on MR merge |
| `src/main.rs` | **Modify** | Spawn preview reconciler task |
| `.sqlx/` | **Modify** | Regenerated offline cache |

---

## Verification

### Automated
1. `just db-migrate && just db-prepare` — migration applies cleanly
2. `just ci` — all tests pass, including new `slugify_branch` tests
3. `just lint` — no clippy warnings

### Manual Testing (requires Kind cluster)
1. Push to feature branch → pipeline succeeds → check `preview_deployments` table
2. Verify K8s namespace created: `kubectl get namespaces | grep preview-`
3. Verify Deployment + Service created in preview namespace
4. Merge MR → verify preview stopped → namespace cleaned up
5. Create preview, wait for TTL → verify auto-cleanup

### Integration Test
```rust
#[sqlx::test(migrations = "migrations")]
async fn preview_created_on_feature_branch_pipeline(pool: PgPool) {
    // Insert project + pipeline (feature branch) + successful step with image
    // Call detect_and_write_deployment()
    // Verify preview_deployments row exists with correct slug
}
```

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| K8s namespace leaks | Orphaned namespaces | TTL + reconciler cleanup; admin can list/delete previews |
| Branch slug collisions | Two branches map to same slug | UNIQUE constraint causes upsert; latest push wins |
| Too many previews | K8s resource exhaustion | Default TTL 24h; configurable per-project; max preview limit possible |
| Slow K8s API | Reconciler bottleneck | 5-preview concurrency limit per reconcile cycle |
| Preview app needs DB/services | Preview fails without dependencies | Out of scope — initial previews are stateless apps only |

---

## Future Enhancements (Not in Scope)

- **Preview URL routing**: Ingress/gateway configuration for `preview-*.example.com`
- **Preview compose**: Support for multi-service previews (app + DB + cache)
- **Per-project TTL configuration**: Column on `projects` table for default TTL
- **Preview status in MR UI**: Show preview URL/status inline in MR detail page
- **Preview resource limits**: Configurable CPU/memory per preview

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New files | 3 (migration pair + preview.rs) |
| Modified files | 5 (Rust) + `.sqlx/` |
| New migrations | 1 |
| Estimated LOC | ~500 |
| New API endpoints | 3 (list, get, delete previews) |
| New unit tests | ~10 (slugify_branch tests) |
