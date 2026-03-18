use sqlx::PgPool;
use uuid::Uuid;

use crate::git::templates::TemplateFile;
use crate::onboarding::presets;
use crate::store::AppState;

/// Demo template files for the `FastAPI` + HTMX shop demo.
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
            path: "CLAUDE.md",
            content: include_str!("templates/CLAUDE.md").to_owned(),
        },
        TemplateFile {
            path: ".claude/commands/dev.md",
            content: include_str!("templates/dev.md").to_owned(),
        },
        TemplateFile {
            path: "README.md",
            content: include_str!("templates/README.md").to_owned(),
        },
    ]
}

/// Create the demo project: git repo, DB row, sample issues, infrastructure,
/// ops repo sync, deployment row, and pipeline trigger.
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

    // Init bare repo with demo template files
    let repo_path = crate::git::repo::init_bare_repo_with_files(
        &state.config.git_repos_path,
        &owner_name,
        project_name,
        "main",
        &demo_project_template_files(),
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

    // Create sample issues
    create_sample_issues(&state.pool, project_id).await?;

    // --- Infrastructure setup (non-best-effort: log errors but continue) ---

    // 1. K8s namespaces + ops repo
    if let Err(e) =
        crate::api::projects::setup_project_infrastructure(state, project_id, &namespace_slug).await
    {
        tracing::warn!(error = %e, "demo project infra setup incomplete");
    }

    // 2. Sync deploy/ from project repo to ops repo
    sync_demo_deploy(state, project_id, &repo_path, &namespace_slug).await;

    // 3. Trigger pipeline on main branch
    // (deployment row is created by the eventbus when the pipeline succeeds
    //  and produces a real image_ref — not before)
    trigger_demo_pipeline(state, project_id, &repo_path_str, owner_id).await;

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

/// Sync `deploy/` directory from project repo to ops repo.
/// Returns the ops_repo_id if successful.
async fn sync_demo_deploy(
    state: &AppState,
    project_id: Uuid,
    repo_path: &std::path::Path,
    namespace_slug: &str,
) -> Option<Uuid> {
    // Look up ops repo for this project
    let ops = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await;

    let (ops_id, ops_path, ops_branch) = match ops {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(%project_id, %namespace_slug, "no ops repo found for demo project");
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to look up ops repo");
            return None;
        }
    };

    // Get HEAD sha of project repo
    let head_sha = match crate::deployer::ops_repo::get_head_sha(repo_path).await {
        Ok(sha) => sha,
        Err(e) => {
            tracing::warn!(error = %e, "failed to get HEAD sha from demo repo");
            return Some(ops_id);
        }
    };

    // Sync deploy/ to ops repo
    let ops_repo_path = std::path::PathBuf::from(&ops_path);
    if let Err(e) = crate::deployer::ops_repo::sync_from_project_repo(
        repo_path,
        &ops_repo_path,
        &ops_branch,
        &head_sha,
    )
    .await
    {
        tracing::warn!(error = %e, "failed to sync deploy/ to ops repo for demo project");
    } else {
        tracing::info!(%project_id, "demo deploy/ synced to ops repo");
    }

    Some(ops_id)
}

/// Trigger a pipeline run on the main branch of the demo project.
async fn trigger_demo_pipeline(
    state: &AppState,
    project_id: Uuid,
    repo_path_str: &str,
    owner_id: Uuid,
) {
    match crate::pipeline::trigger::on_api(
        &state.pool,
        std::path::Path::new(repo_path_str),
        project_id,
        "refs/heads/main",
        owner_id,
    )
    .await
    {
        Ok(pipeline_id) => {
            crate::pipeline::trigger::notify_executor(state, pipeline_id).await;
            tracing::info!(%project_id, %pipeline_id, "demo pipeline triggered");
        }
        Err(e) => {
            tracing::warn!(error = %e, %project_id, "demo pipeline trigger failed");
        }
    }
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

1. **Browse the shop** — The demo is a fully working online shop
2. **Run the pipeline** — Go to Builds tab and trigger a build
3. **Start an agent session** — Try: "Add product search and filtering"
4. **View metrics** — Check Observe > Metrics for `shop.*` business metrics
5. **Check traces** — Observe > Traces shows request flows through the app

Each feature connects to show a complete development workflow."#,
        ),
        (
            "Add product search and filtering",
            "open",
            vec!["enhancement"],
            "The product catalog shows all items in a grid. Add search and category filtering:\n\n\
- Search bar that filters products by name/description\n\
- Category tabs or dropdown to filter by category\n\
- URL query params so filtered views are shareable\n\n\
This is a great first feature to try building with an agent session.",
        ),
        (
            "Add product reviews",
            "open",
            vec!["enhancement"],
            r#"Let customers leave reviews on products:

- Star rating (1-5) + review text
- Display average rating on product cards
- Review list on product detail page
- Database migration to add a `reviews` table

This demonstrates schema evolution and the migration workflow."#,
        ),
        (
            "Set up monitoring alerts",
            "open",
            vec!["ops"],
            "Add monitoring alerts for the shop:\n\n\
- Alert when checkout error rate exceeds 5%\n\
- Alert when order processing latency exceeds 2s\n\
- Alert when product stock drops below 10\n\n\
Configure these in Observe > Alerts using the `shop.*` metrics.",
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
    fn demo_template_file_count() {
        let files = demo_project_template_files();
        assert_eq!(files.len(), 21);
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
    fn demo_template_has_platform_yaml() {
        let files = demo_project_template_files();
        let f = files.iter().find(|f| f.path == ".platform.yaml").unwrap();
        assert!(f.content.contains("pipeline"));
    }

    #[test]
    fn demo_template_has_dockerfile() {
        let files = demo_project_template_files();
        let f = files.iter().find(|f| f.path == "Dockerfile").unwrap();
        assert!(f.content.contains("uvicorn"));
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
    fn demo_template_has_readme() {
        let files = demo_project_template_files();
        let f = files.iter().find(|f| f.path == "README.md").unwrap();
        assert!(f.content.contains("Demo"));
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
