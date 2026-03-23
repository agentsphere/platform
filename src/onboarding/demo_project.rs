use sqlx::PgPool;
use uuid::Uuid;

use crate::git::templates::TemplateFile;
use crate::onboarding::presets;
use crate::store::AppState;

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

/// Full demo app files for Phase 2 — committed on the `feature/shop-app` branch.
#[allow(clippy::too_many_lines)]
pub fn demo_project_template_files() -> Vec<TemplateFile> {
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
            path: ".platform.yaml",
            content: include_str!("templates/platform.yaml").to_owned(),
        },
        TemplateFile {
            path: "Dockerfile",
            content: include_str!("templates/Dockerfile").to_owned(),
        },
        TemplateFile {
            path: "Dockerfile.canary",
            content: include_str!("templates/Dockerfile.canary").to_owned(),
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
            path: "deploy/production.yaml",
            content: include_str!("templates/deploy/production.yaml").to_owned(),
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
            path: "deploy/deployment-stable.yaml",
            content: include_str!("templates/deploy/deployment-stable.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/deployment-canary.yaml",
            content: include_str!("templates/deploy/deployment-canary.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/service-stable.yaml",
            content: include_str!("templates/deploy/service-stable.yaml").to_owned(),
        },
        TemplateFile {
            path: "deploy/service-canary.yaml",
            content: include_str!("templates/deploy/service-canary.yaml").to_owned(),
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

/// Create the demo project in two phases:
///   Phase 1 — bare repo with minimal files on `main`, DB row, sample issues, infra.
///   Phase 2 — feature branch `feature/shop-app` with full demo app, MR, pipeline.
/// Returns `(project_id, project_name)`.
#[tracing::instrument(skip(state), fields(%owner_id), err)]
pub async fn create_demo_project(
    state: &AppState,
    owner_id: Uuid,
) -> Result<(Uuid, String), anyhow::Error> {
    let project_name = "platform-demo";

    // Resolve owner name for the repo path
    let owner_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(owner_id)
        .fetch_one(&state.pool)
        .await?;

    // Resolve workspace
    let workspace_id = crate::workspace::service::get_or_create_default_workspace(
        &state.pool,
        owner_id,
        &owner_name,
        &owner_name,
    )
    .await?;

    // --- Phase 1: Init bare repo with minimal skeleton on main ---
    let repo_path = crate::git::repo::init_bare_repo_with_files(
        &state.config.git_repos_path,
        &owner_name,
        project_name,
        "main",
        &phase1_template_files(),
    )
    .await?;
    let repo_path_str = repo_path.to_string_lossy().to_string();

    // Generate a slug
    let namespace_slug = crate::deployer::namespace::slugify_namespace(project_name);

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

    // K8s namespaces + ops repo
    if let Err(e) =
        crate::api::projects::setup_project_infrastructure(state, project_id, &namespace_slug).await
    {
        tracing::warn!(error = %e, "demo project infra setup incomplete");
    }

    // --- Phase 2: Feature branch with full demo app + MR ---
    create_feature_branch_and_mr(state, project_id, &repo_path, owner_id).await;

    // Store demo project ID in platform_settings
    crate::onboarding::presets::upsert_setting_pub(
        &state.pool,
        "demo_project_id",
        &serde_json::json!(project_id),
    )
    .await?;

    tracing::info!(%project_id, "demo project fully bootstrapped");
    Ok((project_id, project_name.to_owned()))
}

/// Phase 2: Push feature branch with demo app, create MR, trigger pipeline.
async fn create_feature_branch_and_mr(
    state: &AppState,
    project_id: Uuid,
    repo_path: &std::path::Path,
    owner_id: Uuid,
) {
    let branch_name = "feature/shop-app";

    // Create worktree, write files, commit, clean up
    if !commit_feature_branch(repo_path, branch_name).await {
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
    )
    .await;
}

/// Create a git worktree, write all demo files, commit, and clean up.
/// Returns `true` on success.
async fn commit_feature_branch(repo_path: &std::path::Path, branch_name: &str) -> bool {
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

    // Write all demo files to the worktree
    for file in demo_project_template_files() {
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
async fn create_demo_mr_and_pipeline(
    state: &AppState,
    project_id: Uuid,
    repo_path: &std::path::Path,
    owner_id: Uuid,
    branch_name: &str,
    branch_sha: Option<String>,
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
    .bind("feat: Add shop demo app with progressive delivery")
    .bind(
        "Adds the full shop demo application with:\n\
         - FastAPI + HTMX frontend\n\
         - PostgreSQL + Valkey backends\n\
         - Progressive delivery (canary deployments)\n\
         - Feature flags (new_checkout_flow, dark_mode)\n\
         - E2E test infrastructure\n\
         - OpenTelemetry observability",
    )
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
    state: &AppState,
    project_id: Uuid,
    repo_path: &std::path::Path,
    owner_id: Uuid,
    branch_name: &str,
    commit_sha: Option<String>,
) {
    use crate::pipeline::trigger::MrTriggerParams;

    match crate::pipeline::trigger::on_mr(
        &state.pool,
        &MrTriggerParams {
            project_id,
            user_id: owner_id,
            repo_path: repo_path.to_path_buf(),
            source_branch: branch_name.to_string(),
            commit_sha,
            action: "opened".into(),
        },
    )
    .await
    {
        Ok(Some(pipeline_id)) => {
            crate::pipeline::trigger::notify_executor(state, pipeline_id).await;
            tracing::info!(%project_id, %pipeline_id, "demo MR pipeline triggered");
        }
        Ok(None) => tracing::warn!(%project_id, "demo MR pipeline: trigger did not match"),
        Err(e) => tracing::warn!(error = %e, "demo MR pipeline trigger failed"),
    }
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
///
/// These demonstrate the secrets hierarchy and are injected into pipeline pods
/// (scope=pipeline/all) and deploy namespaces (scope=deploy/all).
async fn create_sample_secrets(
    state: &AppState,
    project_id: Uuid,
    owner_id: Uuid,
) -> Result<(), anyhow::Error> {
    use crate::secrets::engine::{CreateSecretParams, create_secret, parse_master_key};

    let master_key_hex = state
        .config
        .master_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("PLATFORM_MASTER_KEY not configured"))?;
    let master_key = parse_master_key(master_key_hex)?;

    // Secrets visible in pipeline pods (build args, test config)
    let secrets = vec![
        ("APP_SECRET_KEY", "demo-secret-key-change-me", "pipeline"),
        (
            "DATABASE_URL",
            "postgresql://demo:demo@platform-demo-db:5432/shop",
            "all",
        ),
        ("VALKEY_URL", "redis://platform-demo-valkey:6379", "all"),
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
            value: b"postgresql://demo:demo@platform-demo-db:5432/shop_staging",
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
            value: b"postgresql://demo:demo@platform-demo-db:5432/shop_production",
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
    state: &AppState,
    admin_id: Uuid,
) -> Result<(), anyhow::Error> {
    // Idempotency: skip if demo project already exists
    if let Ok(Some(_)) = presets::get_setting(&state.pool, "demo_project_id").await {
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
            "Configure the canary deployment pipeline:\n\n\
             1. Review `.platform.yaml` for the canary steps\n\
             2. Check `deploy/` manifests for stable/canary split\n\
             3. Merge the demo MR to trigger first deployment\n\
             4. Use `POST /api/projects/{id}/promote-staging` to promote to production\n\
             5. Monitor canary traffic via deploy releases API",
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

    #[test]
    fn demo_template_file_count() {
        let files = demo_project_template_files();
        assert_eq!(files.len(), 31);
    }

    #[test]
    fn demo_template_has_main_py() {
        let files = demo_project_template_files();
        let main = files.iter().find(|f| f.path == "app/main.py").unwrap();
        assert!(main.content.contains("FastAPI"));
        assert!(main.content.contains("healthz"));
        assert!(main.content.contains("shop"));
    }

    #[test]
    fn demo_template_has_flags_py() {
        let files = demo_project_template_files();
        let f = files.iter().find(|f| f.path == "app/flags.py").unwrap();
        assert!(f.content.contains("evaluate"));
    }

    #[test]
    fn demo_template_has_platform_yaml() {
        let files = demo_project_template_files();
        let f = files.iter().find(|f| f.path == ".platform.yaml").unwrap();
        assert!(f.content.contains("pipeline"));
        assert!(f.content.contains("canary"));
        assert!(f.content.contains("flags"));
    }

    #[test]
    fn demo_template_has_dockerfile() {
        let files = demo_project_template_files();
        let f = files.iter().find(|f| f.path == "Dockerfile").unwrap();
        assert!(f.content.contains("uvicorn"));
    }

    #[test]
    fn demo_template_has_canary_dockerfile() {
        let files = demo_project_template_files();
        let f = files
            .iter()
            .find(|f| f.path == "Dockerfile.canary")
            .unwrap();
        assert!(f.content.contains("canary"));
    }

    #[test]
    fn demo_template_has_deploy_manifests() {
        let files = demo_project_template_files();
        let names: Vec<&str> = files.iter().map(|f| f.path).collect();
        assert!(names.contains(&"deploy/postgres.yaml"));
        assert!(names.contains(&"deploy/valkey.yaml"));
        assert!(names.contains(&"deploy/deployment-stable.yaml"));
        assert!(names.contains(&"deploy/deployment-canary.yaml"));
        assert!(names.contains(&"deploy/service-stable.yaml"));
        assert!(names.contains(&"deploy/service-canary.yaml"));
        assert!(names.contains(&"deploy/production.yaml"));
    }

    #[test]
    fn demo_template_has_testinfra() {
        let files = demo_project_template_files();
        let names: Vec<&str> = files.iter().map(|f| f.path).collect();
        assert!(names.contains(&"testinfra/postgres.yaml"));
        assert!(names.contains(&"testinfra/app.yaml"));
        assert!(names.contains(&"testinfra/service.yaml"));
    }

    #[test]
    fn demo_template_has_tests() {
        let files = demo_project_template_files();
        let f = files
            .iter()
            .find(|f| f.path == "tests-e2e/test_app.py")
            .unwrap();
        assert!(f.content.contains("test_"));
    }

    #[test]
    fn demo_template_paths_unique() {
        let files = demo_project_template_files();
        let mut paths: Vec<&str> = files.iter().map(|f| f.path).collect();
        let len_before = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), len_before, "duplicate template paths");
    }
}
