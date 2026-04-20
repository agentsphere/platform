// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Demo project creation — two-phase bootstrap with feature branch, MR, and pipeline trigger.

use platform_git::TemplateFile;
use sqlx::PgPool;
use uuid::Uuid;

use crate::state::PlatformState;

// ---------------------------------------------------------------------------
// Settings helpers (inline — replaces the presets module dependency)
// ---------------------------------------------------------------------------

pub async fn upsert_setting(
    pool: &PgPool,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO platform_settings (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = $2",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_setting(
    pool: &PgPool,
    key: &str,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar("SELECT value FROM platform_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
}

// ---------------------------------------------------------------------------
// Workspace helper (inline — replaces cross-module dependency)
// ---------------------------------------------------------------------------

async fn get_or_create_workspace(
    pool: &PgPool,
    user_id: Uuid,
    owner_name: &str,
) -> Result<Uuid, anyhow::Error> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM workspaces WHERE owner_id = $1 AND is_active = true ORDER BY created_at LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let ws_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces (id, name, display_name, description, owner_id) \
         VALUES ($1, $2, $3, 'Personal workspace', $4)",
    )
    .bind(ws_id)
    .bind(format!("{owner_name}-personal"))
    .bind(format!("{owner_name}'s workspace"))
    .bind(user_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(ws_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(ws_id)
}

// ---------------------------------------------------------------------------
// Template files
// ---------------------------------------------------------------------------

/// Minimal files for Phase 1 — empty project skeleton on `main` branch.
fn phase1_template_files() -> Vec<TemplateFile> {
    vec![
        TemplateFile {
            path: "README.md",
            content: include_str!("templates/README.md").to_owned(),
        },
        TemplateFile {
            path: "CLAUDE.md",
            content: include_str!("templates/CLAUDE.md").to_owned(),
        },
        TemplateFile {
            path: ".claude/commands/dev.md",
            content: include_str!("templates/dev.md").to_owned(),
        },
    ]
}

/// PR1 demo files: full app + VERSION (v0.1) + deployment-v0.1 + rolling .platform.yaml.
#[allow(clippy::too_many_lines)]
pub fn demo_pr1_template_files() -> Vec<TemplateFile> {
    vec![
        TemplateFile {
            path: "app/main.py",
            content: include_str!("templates/app/main.py").to_owned(),
        },
        TemplateFile {
            path: "app/db.py",
            content: include_str!("templates/app/db.py").to_owned(),
        },
        TemplateFile {
            path: "app/cart.py",
            content: include_str!("templates/app/cart.py").to_owned(),
        },
        TemplateFile {
            path: "app/flags.py",
            content: include_str!("templates/app/flags.py").to_owned(),
        },
        TemplateFile {
            path: "app/__init__.py",
            content: String::new(),
        },
        TemplateFile {
            path: "app/templates/base.html",
            content: include_str!("templates/app/templates/base.html").to_owned(),
        },
        TemplateFile {
            path: "app/templates/catalog.html",
            content: include_str!("templates/app/templates/catalog.html").to_owned(),
        },
        TemplateFile {
            path: "app/templates/product.html",
            content: include_str!("templates/app/templates/product.html").to_owned(),
        },
        TemplateFile {
            path: "app/templates/cart.html",
            content: include_str!("templates/app/templates/cart.html").to_owned(),
        },
        TemplateFile {
            path: "app/templates/orders.html",
            content: include_str!("templates/app/templates/orders.html").to_owned(),
        },
        TemplateFile {
            path: "app/static/style.css",
            content: include_str!("templates/app/static/style.css").to_owned(),
        },
        TemplateFile {
            path: "tests-e2e/test_app.py",
            content: include_str!("templates/tests-e2e/test_app.py").to_owned(),
        },
        TemplateFile {
            path: "requirements.txt",
            content: include_str!("templates/requirements.txt").to_owned(),
        },
        TemplateFile {
            path: "requirements-test.txt",
            content: include_str!("templates/requirements-test.txt").to_owned(),
        },
        TemplateFile {
            path: "VERSION",
            content: include_str!("templates/VERSION").to_owned(),
        },
        TemplateFile {
            path: ".platform.yaml",
            content: include_str!("templates/platform_v0.1.yaml").to_owned(),
        },
        TemplateFile {
            path: "Dockerfile",
            content: include_str!("templates/Dockerfile").to_owned(),
        },
        TemplateFile {
            path: "Dockerfile.test",
            content: include_str!("templates/Dockerfile.test").to_owned(),
        },
        TemplateFile {
            path: "Dockerfile.dev",
            content: include_str!("templates/Dockerfile.dev").to_owned(),
        },
        TemplateFile {
            path: "screenshots/render.py",
            content: include_str!("templates/screenshots/render.py").to_owned(),
        },
        TemplateFile {
            path: "screenshots/capture.py",
            content: include_str!("templates/screenshots/capture.py").to_owned(),
        },
        TemplateFile {
            path: "deploy/postgres.yaml",
            content: include_str!("templates/deploy/postgres.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/valkey.yaml",
            content: include_str!("templates/deploy/valkey.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/deployment-v0.1.yaml",
            content: include_str!("templates/deploy/deployment-v0.1.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/variables_staging.yaml",
            content: include_str!("templates/deploy/variables_staging.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/variables_prod.yaml",
            content: include_str!("templates/deploy/variables_prod.yaml").to_owned(),
        },
        TemplateFile {
            path: "testinfra/postgres.yaml",
            content: include_str!("templates/testinfra/postgres.yaml").to_owned(),
        },
        TemplateFile {
            path: "testinfra/app.yaml",
            content: include_str!("templates/testinfra/app.yaml").to_owned(),
        },
        TemplateFile {
            path: "testinfra/service.yaml",
            content: include_str!("templates/testinfra/service.yaml").to_owned(),
        },
    ]
}

/// PR2 demo files: only changed files for v0.2 canary deployment.
pub fn demo_pr2_template_files() -> Vec<TemplateFile> {
    vec![
        TemplateFile {
            path: "VERSION",
            content: "app=0.2.0\n".to_owned(),
        },
        TemplateFile {
            path: ".platform.yaml",
            content: include_str!("templates/platform_v0.2.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/deployment-v0.2.yaml",
            content: include_str!("templates/deploy/deployment-v0.2.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/traffic-generator.yaml",
            content: include_str!("templates/deploy/traffic-generator.yaml").to_owned(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Demo project creation
// ---------------------------------------------------------------------------

/// Create the demo project in two phases:
///   Phase 1 — bare repo with minimal files on `main`, DB row, sample issues, infra.
///   Phase 2 — feature branch `feature/shop-app` with full demo app, MR, pipeline.
/// Returns `(project_id, project_name)`.
#[tracing::instrument(skip(state), fields(%owner_id), err)]
pub async fn create_demo_project(
    state: &PlatformState,
    owner_id: Uuid,
) -> Result<(Uuid, String), anyhow::Error> {
    let project_name = "platform-demo";

    // Resolve owner name for the repo path
    let owner_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(owner_id)
        .fetch_one(&state.pool)
        .await?;

    // Resolve workspace
    let workspace_id = get_or_create_workspace(&state.pool, owner_id, &owner_name).await?;

    // --- Phase 1: Init bare repo with minimal skeleton on main ---
    let mgr = platform_git::CliGitRepoManager;
    let repo_path = platform_git::GitRepoManager::init_bare_with_files(
        &mgr,
        &state.config.git.git_repos_path,
        &owner_name,
        project_name,
        "main",
        &phase1_template_files(),
    )
    .await?;
    let repo_path_str = repo_path.to_string_lossy().to_string();

    // Generate a slug
    let namespace_slug = platform_k8s::slugify_namespace(project_name)?;

    // Insert project row
    let project_id: Uuid = sqlx::query_scalar(
        r"INSERT INTO projects (name, owner_id, workspace_id, visibility, default_branch,
                                 repo_path, namespace_slug, display_name, description)
           VALUES ($1, $2, $3, 'private', 'main', $4, $5, $6, $7)
           RETURNING id",
    )
    .bind(project_name)
    .bind(owner_id)
    .bind(workspace_id)
    .bind(&repo_path_str)
    .bind(&namespace_slug)
    .bind("Platform Demo")
    .bind(
        "A demo project showcasing the platform's features — pipelines, agents, deploys, and more.",
    )
    .fetch_one(&state.pool)
    .await?;

    // Enable staging for the demo project so gitops_sync writes to staging branch.
    sqlx::query("UPDATE projects SET include_staging = true WHERE id = $1")
        .bind(project_id)
        .execute(&state.pool)
        .await?;

    // Create sample issues
    create_sample_issues(&state.pool, project_id).await?;

    // Create sample secrets (best-effort)
    if let Err(e) = create_sample_secrets(state, project_id, owner_id).await {
        tracing::warn!(error = %e, "demo project sample secrets creation failed");
    }

    // --- Infrastructure setup (best-effort: log errors but continue) ---
    if let Err(e) =
        crate::api::projects::setup_project_infrastructure(state, project_id, &namespace_slug).await
    {
        tracing::warn!(error = %e, "demo project infra setup incomplete");
    }

    // --- Phase 2: Feature branch with full demo app + MR ---
    create_feature_branch_and_mr(state, project_id, &repo_path, owner_id).await;

    // Store demo project ID in platform_settings
    upsert_setting(
        &state.pool,
        "demo_project_id",
        &serde_json::json!(project_id),
    )
    .await?;

    tracing::info!(%project_id, "demo project fully bootstrapped");
    Ok((project_id, project_name.to_owned()))
}

/// Phase 2: Push feature branch with demo app (PR1 — v0.1, rolling deploy).
async fn create_feature_branch_and_mr(
    state: &PlatformState,
    project_id: Uuid,
    repo_path: &std::path::Path,
    owner_id: Uuid,
) {
    let branch_name = "feature/shop-app-v0.1";

    // Create worktree, write PR1 files, commit, clean up
    if !commit_feature_branch(repo_path, branch_name, &demo_pr1_template_files()).await {
        return;
    }

    // Resolve HEAD SHA of the feature branch
    let branch_sha = resolve_branch_sha(repo_path, branch_name).await;

    // Create MR + trigger pipeline
    create_demo_mr_and_pipeline(
        state,
        project_id,
        repo_path,
        owner_id,
        branch_name,
        branch_sha,
        "feat: Add shop demo app (v0.1 — rolling deploy)",
        "Adds the full shop demo application with:\n\
         - FastAPI + HTMX frontend\n\
         - PostgreSQL + Valkey backends\n\
         - Versioned deployment (v0.1)\n\
         - Rolling deploy strategy\n\
         - Feature flags (new_checkout_flow, dark_mode)\n\
         - E2E test infrastructure\n\
         - OpenTelemetry observability",
    )
    .await;
}

/// Create a git worktree, write files, commit, and clean up.
/// Returns `true` on success.
async fn commit_feature_branch(
    repo_path: &std::path::Path,
    branch_name: &str,
    files: &[TemplateFile],
) -> bool {
    let worktree_dir = repo_path
        .parent()
        .unwrap_or(repo_path)
        .join(format!("_demo_feature_{}", Uuid::new_v4()));

    // Clean up from previous runs (no-op on fresh repo).
    // Prune stale worktrees and delete the branch if it exists, making
    // feature-branch creation idempotent after cluster restarts.
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["worktree", "prune"])
        .output()
        .await;
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["branch", "-D", branch_name])
        .output()
        .await;

    let wt_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["worktree", "add", "-b", branch_name])
        .arg(&worktree_dir)
        .arg("main")
        .output()
        .await;

    match &wt_output {
        Err(e) => {
            tracing::warn!(error = %e, "failed to create feature branch worktree");
            return false;
        }
        Ok(out) if !out.status.success() => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr),
                "failed to create feature branch"
            );
            return false;
        }
        _ => {}
    }

    // Write all files to the worktree
    for file in files {
        let dest = worktree_dir.join(file.path);
        if let Some(parent) = dest.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(e) = tokio::fs::write(&dest, &file.content).await {
            tracing::warn!(error = %e, path = file.path, "failed to write demo file");
        }
    }

    // Stage and commit
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .args(["add", "-A"])
        .output()
        .await;

    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .args([
            "commit",
            "-m",
            "feat: add shop demo app with progressive delivery",
        ])
        .output()
        .await;

    // Clean up worktree
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["worktree", "remove", "--force"])
        .arg(&worktree_dir)
        .output()
        .await;
    let _ = tokio::fs::remove_dir_all(&worktree_dir).await;

    true
}

/// Insert the MR row and trigger a pipeline on the feature branch.
#[allow(clippy::too_many_arguments)]
async fn create_demo_mr_and_pipeline(
    state: &PlatformState,
    project_id: Uuid,
    repo_path: &std::path::Path,
    owner_id: Uuid,
    branch_name: &str,
    branch_sha: Option<String>,
    title: &str,
    body: &str,
) {
    // Atomically allocate an MR number
    let mr_number = sqlx::query_scalar::<_, i32>(
        "UPDATE projects SET next_mr_number = next_mr_number + 1 \
         WHERE id = $1 AND is_active = true \
         RETURNING next_mr_number",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await;

    let number = match mr_number {
        Ok(Some(n)) => n,
        Ok(None) => {
            tracing::warn!(%project_id, "project not found when creating demo MR");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, %project_id, "failed to allocate MR number");
            return;
        }
    };

    let mr_result = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO merge_requests \
             (project_id, number, author_id, source_branch, target_branch, title, body, head_sha, \
              auto_merge, auto_merge_by, auto_merge_method) \
         VALUES ($1, $2, $3, $4, 'main', $5, $6, $7, true, $3, 'merge') \
         RETURNING id",
    )
    .bind(project_id)
    .bind(number)
    .bind(owner_id)
    .bind(branch_name)
    .bind(title)
    .bind(body)
    .bind(branch_sha.as_deref())
    .fetch_optional(&state.pool)
    .await;

    match mr_result {
        Ok(Some(_mr_id)) => {
            tracing::info!(%project_id, mr_number = number, "demo MR created");
            trigger_mr_pipeline(
                state,
                project_id,
                repo_path,
                owner_id,
                branch_name,
                branch_sha,
            )
            .await;
        }
        Ok(None) => tracing::warn!(%project_id, "MR insert returned no id"),
        Err(e) => tracing::warn!(error = %e, %project_id, "demo MR creation failed"),
    }
}

/// Trigger a pipeline on the feature branch after MR creation.
async fn trigger_mr_pipeline(
    state: &PlatformState,
    project_id: Uuid,
    repo_path: &std::path::Path,
    owner_id: Uuid,
    branch_name: &str,
    commit_sha: Option<String>,
) {
    use platform_pipeline::trigger::MrTriggerParams;

    match platform_pipeline::trigger::on_mr(
        &state.pool,
        &MrTriggerParams {
            project_id,
            user_id: owner_id,
            repo_path: repo_path.to_path_buf(),
            source_branch: branch_name.to_string(),
            commit_sha,
            action: "opened".into(),
        },
        &state.config.pipeline.kaniko_image,
    )
    .await
    {
        Ok(Some(pipeline_id)) => {
            platform_pipeline::trigger::notify_executor(
                &state.pipeline_notify,
                &state.valkey,
                pipeline_id,
            )
            .await;
            tracing::info!(%project_id, %pipeline_id, "demo MR pipeline triggered");
        }
        Ok(None) => tracing::warn!(%project_id, "demo MR pipeline: trigger did not match"),
        Err(e) => tracing::warn!(error = %e, "demo MR pipeline trigger failed"),
    }
}

/// Create PR2 (v0.2, canary deployment) after PR1 merges.
///
/// Called from eventbus when the demo project's production v0.1 deploy completes.
pub async fn create_demo_pr2(state: &PlatformState, project_id: Uuid, owner_id: Uuid) {
    let repo_path_str: Option<String> =
        sqlx::query_scalar("SELECT repo_path FROM projects WHERE id = $1 AND is_active = true")
            .bind(project_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();

    let Some(repo_path_str) = repo_path_str else {
        tracing::warn!(%project_id, "cannot create PR2: project repo_path not found");
        return;
    };
    let repo_path = std::path::PathBuf::from(&repo_path_str);

    let branch_name = "feature/shop-app-v0.2";

    if !commit_feature_branch(&repo_path, branch_name, &demo_pr2_template_files()).await {
        tracing::warn!(%project_id, "demo PR2: feature branch commit failed");
        return;
    }

    let branch_sha = resolve_branch_sha(&repo_path, branch_name).await;

    create_demo_mr_and_pipeline(
        state,
        project_id,
        &repo_path,
        owner_id,
        branch_name,
        branch_sha,
        "feat: Add v0.2 with canary deployment",
        "Adds canary deployment for v0.2:\n\
         - VERSION bumped to 0.2.0\n\
         - New deployment-v0.2 manifest\n\
         - Canary deploy strategy (v0.1 → v0.2)\n\
         - Traffic shifting: 10% → 25% → 50% → 100%",
    )
    .await;

    tracing::info!(%project_id, "demo PR2 (v0.2 canary) created");
}

/// Resolve the HEAD SHA of a branch in a bare repo.
async fn resolve_branch_sha(repo_path: &std::path::Path, branch: &str) -> Option<String> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", &format!("refs/heads/{branch}")])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Create sample secrets for the demo project.
async fn create_sample_secrets(
    state: &PlatformState,
    project_id: Uuid,
    owner_id: Uuid,
) -> Result<(), anyhow::Error> {
    use platform_secrets::engine::{CreateSecretParams, create_secret, parse_master_key};

    let master_key_hex = state
        .config
        .secrets
        .master_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("PLATFORM_MASTER_KEY not configured"))?;
    let master_key = parse_master_key(master_key_hex)?;

    // Secrets visible in pipeline pods (build args, test config)
    let secrets = vec![
        ("APP_SECRET_KEY", "demo-secret-key-change-me", "pipeline"),
        (
            "DATABASE_URL",
            "postgresql://app:changeme@platform-demo-db:5432/app",
            "all",
        ),
        ("VALKEY_URL", "redis://platform-demo-cache:6379", "all"),
        (
            "SENTRY_DSN",
            "https://examplePublicKey@o0.ingest.sentry.io/0",
            "staging",
        ),
    ];

    for (name, value, scope) in &secrets {
        create_secret(
            &state.pool,
            &master_key,
            CreateSecretParams {
                project_id: Some(project_id),
                workspace_id: None,
                environment: None,
                name,
                value: value.as_bytes(),
                scope,
                created_by: owner_id,
            },
        )
        .await?;
    }

    // Environment-specific overrides
    create_secret(
        &state.pool,
        &master_key,
        CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: Some("staging"),
            name: "DATABASE_URL",
            value: b"postgresql://app:changeme@platform-demo-db:5432/app",
            scope: "staging",
            created_by: owner_id,
        },
    )
    .await?;

    create_secret(
        &state.pool,
        &master_key,
        CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: Some("production"),
            name: "DATABASE_URL",
            value: b"postgresql://app:changeme@platform-demo-db:5432/app",
            scope: "prod",
            created_by: owner_id,
        },
    )
    .await?;

    tracing::info!(%project_id, "demo project sample secrets created");
    Ok(())
}

/// Create demo project if it doesn't already exist. Idempotent.
/// Designed to be spawned as a background task on fresh boot.
#[tracing::instrument(skip(state), fields(%admin_id), err)]
pub async fn create_and_trigger_demo(
    state: &PlatformState,
    admin_id: Uuid,
) -> Result<(), anyhow::Error> {
    // Idempotency: skip if demo project already exists
    if let Ok(Some(_)) = get_setting(&state.pool, "demo_project_id").await {
        tracing::info!("demo project already exists, skipping auto-creation");
        return Ok(());
    }

    create_demo_project(state, admin_id).await?;
    Ok(())
}

/// Create sample issues for the demo project.
async fn create_sample_issues(pool: &PgPool, project_id: Uuid) -> Result<(), anyhow::Error> {
    // Get a consistent author_id (the project owner)
    let author_id: Uuid = sqlx::query_scalar("SELECT owner_id FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(pool)
        .await?;

    let issues = vec![
        (
            "Explore the platform",
            "open",
            vec!["documentation"],
            r#"Welcome to the platform! Here's how to get started:

1. **Review the demo MR** — Check the open merge request for the shop app
2. **Run the pipeline** — Go to Builds tab and trigger a build
3. **Start an agent session** — Try: "Add product search and filtering"
4. **View metrics** — Check Observe > Metrics for `shop.*` business metrics
5. **Check traces** — Observe > Traces shows request flows through the app

Each feature connects to show a complete development workflow."#,
        ),
        (
            "Set up progressive delivery pipeline",
            "open",
            vec!["enhancement", "devops"],
            "Configure the progressive delivery pipeline:\n\n\
             1. Review `.platform.yaml` for the rolling deploy steps\n\
             2. Check `deploy/` manifests for v0.1/v0.2 versioned deployments\n\
             3. Merge the demo MR to trigger first deployment\n\
             4. Use `POST /api/projects/{id}/promote-staging` to promote to production\n\
             5. Monitor deployment progress via deploy releases API",
        ),
        (
            "Add feature flag for new checkout flow",
            "open",
            vec!["enhancement"],
            "Use the feature flags system to gate the new checkout flow:\n\n\
             1. Toggle `new_checkout_flow` flag via API\n\
             2. Add percentage rollout rule\n\
             3. Test with `x-experiment: treatment` header\n\
             4. Monitor conversion metrics",
        ),
        (
            "Configure alert rules for canary monitoring",
            "open",
            vec!["enhancement", "observability"],
            "Set up alert rules to monitor canary deployments:\n\n\
             1. Create error rate alert (> 5% triggers rollback)\n\
             2. Create latency alert (p99 > 500ms holds progression)\n\
             3. Review `shop.revenue_cents` metric for business impact\n\
             4. Test with synthetic traffic",
        ),
    ];

    for (title, status, labels, body) in issues {
        // Atomically increment the project's issue counter
        let number: i32 = sqlx::query_scalar(
            "UPDATE projects SET next_issue_number = next_issue_number + 1 WHERE id = $1 RETURNING next_issue_number",
        )
        .bind(project_id)
        .fetch_one(pool)
        .await?;

        let labels_arr: Vec<String> = labels.into_iter().map(String::from).collect();

        sqlx::query(
            r"INSERT INTO issues (project_id, number, title, body, status, author_id, labels)
               VALUES ($1, $2, $3, $4, $5, $6, $7::text[])",
        )
        .bind(project_id)
        .bind(number)
        .bind(title)
        .bind(body)
        .bind(status)
        .bind(author_id)
        .bind(&labels_arr)
        .execute(pool)
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase1_template_file_count() {
        let files = phase1_template_files();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn phase1_has_readme() {
        let files = phase1_template_files();
        let f = files.iter().find(|f| f.path == "README.md").unwrap();
        assert!(f.content.contains("Demo"));
    }

    #[test]
    fn phase1_has_claude_md() {
        let files = phase1_template_files();
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("Platform Demo"));
    }

    #[test]
    fn phase1_has_dev_command() {
        let files = phase1_template_files();
        let f = files
            .iter()
            .find(|f| f.path == ".claude/commands/dev.md")
            .unwrap();
        assert!(f.content.contains("STEP"));
    }

    // -- PR1 template tests --

    #[test]
    fn pr1_template_file_count() {
        let files = demo_pr1_template_files();
        assert_eq!(files.len(), 29);
    }

    #[test]
    fn pr1_template_has_main_py() {
        let files = demo_pr1_template_files();
        let main = files.iter().find(|f| f.path == "app/main.py").unwrap();
        assert!(main.content.contains("FastAPI"));
        assert!(main.content.contains("healthz"));
        assert!(main.content.contains("shop"));
    }

    #[test]
    fn pr1_template_has_flags_py() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "app/flags.py").unwrap();
        assert!(f.content.contains("evaluate"));
    }

    #[test]
    fn pr1_template_has_version_file() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "VERSION").unwrap();
        assert!(f.content.contains("app=0.1.0"));
    }

    #[test]
    fn pr1_template_has_platform_yaml() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == ".platform.yaml").unwrap();
        assert!(f.content.contains("pipeline"));
        assert!(f.content.contains("flags"));
        assert!(!f.content.contains("canary"));
    }

    #[test]
    fn pr1_template_has_dockerfile() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "Dockerfile").unwrap();
        assert!(f.content.contains("uvicorn"));
    }

    #[test]
    fn pr1_template_has_deploy_manifests() {
        let files = demo_pr1_template_files();
        let names: Vec<&str> = files.iter().map(|f| f.path).collect();
        assert!(names.contains(&"deploy/postgres.yaml"));
        assert!(names.contains(&"deploy/valkey.yaml"));
        assert!(names.contains(&"deploy/deployment-v0.1.yaml"));
    }

    #[test]
    fn pr1_template_has_testinfra() {
        let files = demo_pr1_template_files();
        let names: Vec<&str> = files.iter().map(|f| f.path).collect();
        assert!(names.contains(&"testinfra/postgres.yaml"));
        assert!(names.contains(&"testinfra/app.yaml"));
        assert!(names.contains(&"testinfra/service.yaml"));
    }

    #[test]
    fn pr1_template_has_tests() {
        let files = demo_pr1_template_files();
        let f = files
            .iter()
            .find(|f| f.path == "tests-e2e/test_app.py")
            .unwrap();
        assert!(f.content.contains("test_"));
    }

    #[test]
    fn pr1_template_paths_unique() {
        let files = demo_pr1_template_files();
        let mut paths: Vec<&str> = files.iter().map(|f| f.path).collect();
        let len_before = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), len_before, "duplicate template paths");
    }

    // -- PR2 template tests --

    #[test]
    fn pr2_template_file_count() {
        let files = demo_pr2_template_files();
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn pr2_template_has_version_file() {
        let files = demo_pr2_template_files();
        let f = files.iter().find(|f| f.path == "VERSION").unwrap();
        assert!(f.content.contains("app=0.2.0"));
    }

    #[test]
    fn pr2_template_has_canary_platform_yaml() {
        let files = demo_pr2_template_files();
        let f = files.iter().find(|f| f.path == ".platform.yaml").unwrap();
        assert!(f.content.contains("canary"));
        assert!(f.content.contains("platform-demo-app-v0-1"));
        assert!(f.content.contains("platform-demo-app-v0-2"));
    }

    #[test]
    fn pr2_template_has_deployment_v02() {
        let files = demo_pr2_template_files();
        let f = files
            .iter()
            .find(|f| f.path == "deploy/deployment-v0.2.yaml")
            .unwrap();
        assert!(f.content.contains("app-v0-2"));
    }

    #[test]
    fn pr2_template_has_traffic_generator() {
        let files = demo_pr2_template_files();
        let f = files
            .iter()
            .find(|f| f.path == "deploy/traffic-generator.yaml")
            .unwrap();
        assert!(!f.content.is_empty());
    }

    #[test]
    fn pr2_template_paths_unique() {
        let files = demo_pr2_template_files();
        let mut paths: Vec<&str> = files.iter().map(|f| f.path).collect();
        let len_before = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), len_before, "duplicate template paths in PR2");
    }

    #[test]
    fn pr1_template_no_empty_content_except_init_py() {
        let files = demo_pr1_template_files();
        for f in &files {
            if f.path == "app/__init__.py" {
                assert!(f.content.is_empty(), "__init__.py should be empty");
            } else {
                assert!(!f.content.is_empty(), "{} should have content", f.path);
            }
        }
    }

    #[test]
    fn pr1_template_has_dockerfile_dev() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "Dockerfile.dev").unwrap();
        assert!(!f.content.is_empty());
    }

    #[test]
    fn pr1_template_has_dockerfile_test() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "Dockerfile.test").unwrap();
        assert!(!f.content.is_empty());
    }

    #[test]
    fn pr1_template_has_requirements() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "requirements.txt").unwrap();
        assert!(f.content.contains("fastapi") || f.content.contains("uvicorn"));
    }

    #[test]
    fn pr1_template_has_cart_py() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "app/cart.py").unwrap();
        assert!(!f.content.is_empty());
    }

    #[test]
    fn pr1_template_has_db_py() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "app/db.py").unwrap();
        assert!(!f.content.is_empty());
    }

    #[test]
    fn pr1_template_has_html_templates() {
        let files = demo_pr1_template_files();
        let html_files: Vec<_> = files
            .iter()
            .filter(|f| {
                f.path.contains("templates/")
                    && std::path::Path::new(f.path)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
            })
            .collect();
        assert!(
            html_files.len() >= 4,
            "should have at least 4 HTML templates"
        );
    }

    #[test]
    fn pr1_template_has_static_css() {
        let files = demo_pr1_template_files();
        let f = files
            .iter()
            .find(|f| f.path == "app/static/style.css")
            .unwrap();
        assert!(!f.content.is_empty());
    }

    #[test]
    fn pr1_template_has_screenshot_scripts() {
        let files = demo_pr1_template_files();
        let render = files
            .iter()
            .find(|f| f.path == "screenshots/render.py")
            .unwrap();
        assert!(render.content.contains("SEED_PRODUCTS"));
        assert!(render.content.contains("MockRequest"));

        let capture = files
            .iter()
            .find(|f| f.path == "screenshots/capture.py")
            .unwrap();
        assert!(capture.content.contains("write_configs"));
        assert!(capture.content.contains("config.json"));
    }

    #[test]
    fn pr1_template_dev_image_has_playwright() {
        let files = demo_pr1_template_files();
        let f = files.iter().find(|f| f.path == "Dockerfile.dev").unwrap();
        assert!(f.content.to_lowercase().contains("playwright"));
        assert!(f.content.to_lowercase().contains("chromium"));
    }

    #[test]
    fn pr1_template_has_deploy_variables() {
        let files = demo_pr1_template_files();
        let names: Vec<&str> = files.iter().map(|f| f.path).collect();
        assert!(names.contains(&"deploy/variables_staging.yaml"));
        assert!(names.contains(&"deploy/variables_prod.yaml"));
    }

    #[test]
    fn phase1_template_paths_unique() {
        let files = phase1_template_files();
        let mut paths: Vec<&str> = files.iter().map(|f| f.path).collect();
        let len_before = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(
            paths.len(),
            len_before,
            "duplicate template paths in phase1"
        );
    }

    #[test]
    fn pr2_version_differs_from_pr1() {
        let pr1_files = demo_pr1_template_files();
        let pr2_files = demo_pr2_template_files();
        let pr1_version = pr1_files.iter().find(|f| f.path == "VERSION").unwrap();
        let pr2_version = pr2_files.iter().find(|f| f.path == "VERSION").unwrap();
        assert_ne!(pr1_version.content, pr2_version.content);
    }

    #[test]
    fn pr2_platform_yaml_differs_from_pr1() {
        let pr1_files = demo_pr1_template_files();
        let pr2_files = demo_pr2_template_files();
        let pr1_yaml = pr1_files
            .iter()
            .find(|f| f.path == ".platform.yaml")
            .unwrap();
        let pr2_yaml = pr2_files
            .iter()
            .find(|f| f.path == ".platform.yaml")
            .unwrap();
        assert_ne!(pr1_yaml.content, pr2_yaml.content);
    }
}
