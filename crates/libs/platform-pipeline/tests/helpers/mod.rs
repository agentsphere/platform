// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Shared test helpers for `platform-pipeline` integration tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use platform_pipeline::config::PipelineConfig;
use platform_pipeline::state::{MockPipelineServices, NoopHeartbeat, PipelineState};

// ---------------------------------------------------------------------------
// `PipelineState` builder
// ---------------------------------------------------------------------------

/// Build a `PipelineState<MockPipelineServices>` with real PG, Valkey, K8s, and
/// an in-memory opendal `Memory` backend.
pub async fn test_pipeline_state(pool: PgPool) -> PipelineState<MockPipelineServices> {
    let valkey = valkey_pool().await;
    let kube = kube::Client::try_default()
        .await
        .expect("kube client (is the Kind cluster running?)");
    let minio = opendal::Operator::new(opendal::services::Memory::default())
        .expect("memory operator")
        .finish();

    PipelineState {
        pool,
        kube,
        valkey,
        minio,
        config: test_pipeline_config(),
        pipeline_notify: Arc::new(tokio::sync::Notify::new()),
        task_heartbeat: Arc::new(NoopHeartbeat),
        services: MockPipelineServices::default(),
    }
}

/// Default `PipelineConfig` for tests.
pub fn test_pipeline_config() -> PipelineConfig {
    PipelineConfig {
        kaniko_image: "gcr.io/kaniko-project/executor:latest".into(),
        git_clone_image: "alpine/git:latest".into(),
        platform_api_url: "http://platform-test:8080".into(),
        platform_namespace: "platform".into(),
        ns_prefix: None,
        gateway_namespace: "gateway".into(),
        registry_url: Some("registry.local:5000".into()),
        node_registry_url: None,
        pipeline_timeout_secs: 60,
        pipeline_max_parallel: 2,
        dev_mode: true,
        master_key: None,
        ops_repos_path: "/tmp/platform-test-ops".into(),
        proxy_binary_path: None,
        pipeline_namespace: "platform-pipelines".into(),
        max_artifact_file_bytes: 50_000_000,
        max_artifact_total_bytes: 200_000_000,
    }
}

// ---------------------------------------------------------------------------
// Valkey connection
// ---------------------------------------------------------------------------

async fn valkey_pool() -> fred::clients::Pool {
    use fred::interfaces::ClientLike;
    let url = std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let url = url.replace("redis://:", "redis://default:");
    let config = fred::types::config::Config::from_url(&url).expect("invalid VALKEY_URL");
    let pool =
        fred::clients::Pool::new(config, None, None, None, 1).expect("valkey pool creation failed");
    pool.init().await.expect("valkey connection failed");
    pool
}

// ---------------------------------------------------------------------------
// DB seed helpers
// ---------------------------------------------------------------------------

pub async fn seed_user(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type)
         VALUES ($1, $2, $3, 'not-a-hash', 'human')",
    )
    .bind(id)
    .bind(name)
    .bind(format!("{name}@test.local"))
    .execute(pool)
    .await
    .expect("seed user");
    id
}

pub async fn seed_workspace(pool: &PgPool, owner_id: Uuid) -> Uuid {
    let ws_id = Uuid::new_v4();
    let name = format!("ws-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO workspaces (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(ws_id)
        .bind(&name)
        .bind(owner_id)
        .execute(pool)
        .await
        .expect("seed workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(ws_id)
    .bind(owner_id)
    .execute(pool)
    .await
    .expect("seed workspace owner");
    ws_id
}

pub async fn seed_project(pool: &PgPool, owner_id: Uuid, workspace_id: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let name = format!("proj-{}", &id.to_string()[..8]);
    let slug = format!("slug-{}", &id.to_string()[..8]);
    sqlx::query(
        "INSERT INTO projects (id, owner_id, workspace_id, name, namespace_slug)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(owner_id)
    .bind(workspace_id)
    .bind(&name)
    .bind(&slug)
    .execute(pool)
    .await
    .expect("seed project");
    (id, name)
}

/// Insert a pipeline row directly. Returns the pipeline ID.
pub async fn seed_pipeline(pool: &PgPool, project_id: Uuid, user_id: Uuid, status: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, trigger, git_ref, status, triggered_by)
         VALUES ($1, $2, 'push', 'refs/heads/main', $3, $4)",
    )
    .bind(id)
    .bind(project_id)
    .bind(status)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed pipeline");
    id
}

/// Insert a pipeline step row directly. Returns the step ID.
pub async fn seed_step(
    pool: &PgPool,
    pipeline_id: Uuid,
    project_id: Uuid,
    step_order: i32,
    name: &str,
    status: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipeline_steps (id, pipeline_id, project_id, step_order, name, image, commands, status)
         VALUES ($1, $2, $3, $4, $5, 'alpine:3.19', ARRAY['echo hello'], $6)",
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(project_id)
    .bind(step_order)
    .bind(name)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed step");
    id
}

// ---------------------------------------------------------------------------
// Git repo helpers
// ---------------------------------------------------------------------------

/// Create a bare git repo in a temp directory with a `.platform.yaml` file.
/// Returns `(repo_path, working_copy_path)`.
pub async fn create_repo_with_platform_yaml(yaml_content: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("pl-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();

    let bare_path = base.join("bare.git");
    let work_path = base.join("work");

    // Create bare repo
    git_cmd(&bare_path, &["init", "--bare", bare_path.to_str().unwrap()]).await;

    // Create working copy and add .platform.yaml
    git_cmd(
        &work_path,
        &[
            "clone",
            bare_path.to_str().unwrap(),
            work_path.to_str().unwrap(),
        ],
    )
    .await;
    git_cmd(&work_path, &["config", "user.email", "test@test.local"]).await;
    git_cmd(&work_path, &["config", "user.name", "test"]).await;

    let yaml_path = work_path.join(".platform.yaml");
    std::fs::write(&yaml_path, yaml_content).unwrap();

    git_cmd(&work_path, &["add", "."]).await;
    git_cmd(&work_path, &["commit", "-m", "initial"]).await;
    git_cmd(&work_path, &["push", "origin", "main"]).await;

    (bare_path, work_path)
}

/// Run a git command in the given directory.
async fn git_cmd(dir: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .expect("git command failed to execute");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("fatal"),
            "git {args:?} failed in {}: {stderr}",
            dir.display(),
        );
    }
}

/// Minimal `.platform.yaml` with a single echo step.
pub const MINIMAL_PLATFORM_YAML: &str = r"
pipeline:
  steps:
    - name: test-step
      image: alpine:3.19
      commands:
        - echo hello
";
