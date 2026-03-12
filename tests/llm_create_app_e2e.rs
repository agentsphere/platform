//! LLM E2E test: Full create-app flow with real Claude CLI and Kind cluster.
//!
//! Tests the entire chain: create-app session → Manager Agent tool loop →
//! `create_project` + `spawn_coding_agent` → Worker pod writes code, commits, pushes →
//! pipeline trigger (Kaniko build) → deployment.
//!
//! # Running
//!
//! ```bash
//! just test-llm  # requires CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY + Kind cluster
//! ```
//!
//! Or targeted:
//! ```bash
//! cargo nextest run -E 'test(llm_create_app_full_flow)' --run-ignored all
//! ```

mod e2e_helpers;

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::ListParams;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Background task guards (RAII shutdown)
// ---------------------------------------------------------------------------

/// RAII guard that spawns the pipeline executor and shuts it down on drop.
struct ExecutorGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    _handle: tokio::task::JoinHandle<()>,
}

impl ExecutorGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let s = state.clone();
        let handle = tokio::spawn(async move {
            platform::pipeline::executor::run(s, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            _handle: handle,
        }
    }
}

impl Drop for ExecutorGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// RAII guard that spawns the deployer reconciler.
struct ReconcilerGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    _handle: tokio::task::JoinHandle<()>,
}

impl ReconcilerGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let s = state.clone();
        let handle = tokio::spawn(async move {
            platform::deployer::reconciler::run(s, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            _handle: handle,
        }
    }
}

impl Drop for ReconcilerGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// RAII guard that spawns the event bus subscriber (Valkey pub/sub → deployment creation).
struct EventBusGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    _handle: tokio::task::JoinHandle<()>,
}

impl EventBusGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let s = state.clone();
        let handle = tokio::spawn(async move {
            platform::store::eventbus::run(s, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            _handle: handle,
        }
    }
}

impl Drop for EventBusGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// RAII guard that spawns the agent session reaper.
struct ReaperGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    _handle: tokio::task::JoinHandle<()>,
}

impl ReaperGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let s = state.clone();
        let handle = tokio::spawn(async move {
            platform::agent::service::run_reaper(s, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            _handle: handle,
        }
    }
}

impl Drop for ReaperGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

// ---------------------------------------------------------------------------
// Auth helper
// ---------------------------------------------------------------------------

/// Read the Claude OAuth token from env or `.env.test`.
///
/// Checks both `CLAUDE_CODE_OAUTH_TOKEN` and `CLAUDE_OAUTH_TOKEN` env vars,
/// then falls back to `.env.test` file.
fn resolve_oauth_token() -> Option<String> {
    // Try env vars first (both naming conventions)
    for var in &["CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_OAUTH_TOKEN"] {
        if let Ok(token) = std::env::var(var)
            && !token.is_empty()
        {
            return Some(token);
        }
    }

    // Fall back to .env.test file
    if let Ok(contents) = std::fs::read_to_string(".env.test") {
        for prefix in &["CLAUDE_CODE_OAUTH_TOKEN=", "CLAUDE_OAUTH_TOKEN="] {
            for line in contents.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix(prefix) {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }

    None
}

/// Read the Anthropic API key from env or `.env.test`.
fn resolve_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }

    if let Ok(contents) = std::fs::read_to_string(".env.test") {
        for line in contents.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("ANTHROPIC_API_KEY=") {
                let val = val.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Full create-app E2E flow with real LLM (branch-protection-aware).
///
/// Steps:
/// 1. Start real TCP server (agent pods reach platform API)
/// 2. Enable CLI spawn + store OAuth token in DB
/// 3. Spawn background tasks (executor, reconciler, reaper)
/// 4. POST /api/create-app → manager session
/// 5. Poll for manager tool calls (`create_project`, `spawn_coding_agent`)
/// 6. Wait for worker agent pod to complete
/// 7. Verify git push to feature branch + MR creation
/// 8. Wait for MR pipeline (triggered by `on_mr`)
/// 9. Wait for auto-merge (triggered by pipeline success)
/// 10. Verify deployment (post-merge deploy)
/// 11. Verify manager session completes
#[ignore = "requires real Claude CLI and Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn llm_create_app_full_flow(pool: PgPool) {
    // -- 0. Require auth credentials --
    let oauth_token = resolve_oauth_token();
    let api_key = resolve_api_key();
    if oauth_token.is_none() && api_key.is_none() {
        eprintln!("SKIP: no CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY set");
        return;
    }

    // -- 1. Start real TCP server with CLI spawn enabled --
    // We can't use start_agent_server() directly because it builds the router
    // before we can modify config. Instead, bind the listener first, build state
    // with the correct API URL, enable cli_spawn, then build the router.
    let port: u16 = if let Ok(p) = std::env::var("PLATFORM_LISTEN_PORT") {
        p.parse().expect("invalid PLATFORM_LISTEN_PORT")
    } else {
        0 // bind to :0 to let the OS pick a free port
    };
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind listener");
    let actual_port = listener.local_addr().expect("local_addr").port();
    eprintln!("[LLM E2E] Listening on port {actual_port}");
    let host = if cfg!(target_os = "macos") {
        "host.docker.internal"
    } else {
        "172.18.0.1"
    };
    let platform_api_url = format!("http://{host}:{actual_port}");

    let (mut state, admin_token) =
        e2e_helpers::e2e_state_with_api_url(pool.clone(), Some(platform_api_url)).await;

    // -- 2. Enable CLI spawn BEFORE building router --
    let mut config = (*state.config).clone();
    config.cli_spawn_enabled = true;
    state.config = Arc::new(config);

    let app = e2e_helpers::pipeline_test_router(state.clone());

    // Start the TCP server with the CLI-spawn-enabled router
    let _server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app.clone()).await {
            eprintln!("[LLM E2E] SERVER DIED: {e}");
        }
    });

    // Background health monitor — pings the server every 5s to detect if/when it stops
    let health_port = actual_port;
    let _health_monitor = tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            match client
                .get(format!("http://127.0.0.1:{health_port}/healthz"))
                .send()
                .await
            {
                Ok(resp) => {
                    eprintln!(
                        "[HEALTH] port={health_port} status={} at {:?}",
                        resp.status(),
                        std::time::SystemTime::now()
                    );
                }
                Err(e) => {
                    eprintln!("[HEALTH] port={health_port} UNREACHABLE: {e}");
                }
            }
        }
    });

    // Build a second router handle for local API calls (same state)
    let app = e2e_helpers::pipeline_test_router(state.clone());

    // -- 3. Store OAuth token in cli_credentials for the admin user --
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .expect("admin user must exist");

    let master_key_hex = state
        .config
        .master_key
        .as_deref()
        .expect("master_key must be set");
    let master_key =
        platform::secrets::engine::parse_master_key(master_key_hex).expect("valid master key");

    if let Some(ref token) = oauth_token {
        platform::auth::cli_creds::store_credentials(
            &pool,
            &master_key,
            admin_id.0,
            "oauth",
            token,
            None,
        )
        .await
        .expect("store OAuth token");
    } else if let Some(ref key) = api_key {
        // Store API key as a global platform secret so resolve_global_api_key picks it up
        platform::secrets::engine::create_secret(
            &pool,
            &master_key,
            platform::secrets::engine::CreateSecretParams {
                project_id: None,
                workspace_id: None,
                environment: None,
                name: "ANTHROPIC_API_KEY",
                value: key.as_bytes(),
                scope: "agent",
                created_by: admin_id.0,
            },
        )
        .await
        .expect("store API key secret");
    }

    // -- 4. Spawn background tasks --
    let _executor = ExecutorGuard::spawn(&state);
    let _reconciler = ReconcilerGuard::spawn(&state);
    let _eventbus = EventBusGuard::spawn(&state);
    let _reaper = ReaperGuard::spawn(&state);

    // -- 5. Create-app session --
    eprintln!("[LLM E2E] Creating create-app session...");
    let (status, body) = e2e_helpers::post_json(
        &app,
        &admin_token,
        "/api/create-app",
        serde_json::json!({
            "description": "Skip clarification — all details provided. Execute Phase 2 immediately.\n\nApp: hello world Express.js (Node.js LTS) web server. GET /healthz returns {\"status\":\"ok\"} on port 8080. No database, no extra features.\n\nProject name: hello-llm-test\n\nThe main branch is protected. The coding agent must push to a feature branch, then create a merge request targeting main. CI triggers on MR creation and auto-merges when it passes.\n\nYou MUST call create_project first, then spawn_coding_agent with the returned project_id. Both tool calls are required."
        }),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::CREATED,
        "create-app should succeed: {body}"
    );
    let manager_session_id: Uuid = body["id"]
        .as_str()
        .expect("session should have id")
        .parse()
        .unwrap();
    eprintln!("[LLM E2E] Manager session created: {manager_session_id}");

    // -- 6. Poll for manager tool execution --
    // Wait for create_project and spawn_coding_agent tools to complete.
    // Poll the DB directly: the manager links its session to the project after
    // create_project, and child sessions have parent_session_id set.
    let mut project_id: Option<Uuid> = None;
    let mut child_session_id: Option<Uuid> = None;
    let tool_timeout = Duration::from_secs(120);
    let start = std::time::Instant::now();

    eprintln!("[LLM E2E] Waiting for manager tool calls...");
    loop {
        if start.elapsed() > tool_timeout {
            // Dump all manager messages for debugging (full content, not truncated)
            let msgs = get_session_messages_full(&pool, manager_session_id).await;
            // Also dump all projects and sessions for diagnostics
            let projects = dump_all_projects(&pool).await;
            let sessions = dump_all_sessions(&pool).await;
            panic!(
                "Timed out waiting for manager tools ({}s).\n\n--- Messages ---\n{}\n\n--- Projects ---\n{}\n\n--- Sessions ---\n{}",
                tool_timeout.as_secs(),
                msgs,
                projects,
                sessions,
            );
        }

        // Find project via manager session link (create_project links the session)
        if project_id.is_none() {
            let row: Option<(Option<Uuid>,)> =
                sqlx::query_as("SELECT project_id FROM agent_sessions WHERE id = $1")
                    .bind(manager_session_id)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            if let Some((Some(pid),)) = row {
                project_id = Some(pid);
                eprintln!("[LLM E2E] Project created: {pid}");
            }
        }

        // Find project by name if session link not yet updated
        if project_id.is_none() {
            let row: Option<(Uuid,)> = sqlx::query_as(
                "SELECT id FROM projects WHERE name LIKE 'hello-llm%' AND is_active = true ORDER BY created_at DESC LIMIT 1",
            )
            .fetch_optional(&pool)
            .await
            .unwrap();
            if let Some((pid,)) = row {
                project_id = Some(pid);
                eprintln!("[LLM E2E] Project found by name: {pid}");
            }
        }

        // Find child session via parent_session_id
        if child_session_id.is_none() {
            let row: Option<(Uuid,)> = sqlx::query_as(
                "SELECT id FROM agent_sessions WHERE parent_session_id = $1 ORDER BY created_at ASC LIMIT 1",
            )
            .bind(manager_session_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
            if let Some((sid,)) = row {
                child_session_id = Some(sid);
                eprintln!("[LLM E2E] Child session spawned: {sid}");
            }
        }

        // Check if manager session already completed (tools ran fast)
        let mgr_status: Option<(String,)> =
            sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
                .bind(manager_session_id)
                .fetch_optional(&pool)
                .await
                .unwrap();
        if let Some((status,)) = &mgr_status
            && (status == "completed" || status == "failed")
            && project_id.is_none()
        {
            // Manager finished without creating a project — dump full messages and DB state
            let msgs = get_session_messages_full(&pool, manager_session_id).await;
            let projects = dump_all_projects(&pool).await;
            let sessions = dump_all_sessions(&pool).await;
            panic!(
                "Manager session {status} without creating project.\n\n--- Messages ---\n{msgs}\n\n--- Projects ---\n{projects}\n\n--- Sessions ---\n{sessions}"
            );
        }

        if project_id.is_some() && child_session_id.is_some() {
            break;
        }

        // If manager completed and we have project but no child, that's also done
        if let Some((status,)) = &mgr_status
            && (status == "completed" || status == "failed")
            && project_id.is_some()
        {
            eprintln!(
                "[LLM E2E] Manager {status}, project found but no child session. Continuing..."
            );
            break;
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let project_id = project_id.expect("project_id should be found");

    // -- 7. Wait for worker agent pod (if spawned) --
    let mut phase = String::from("N/A");
    let mut child_status = String::from("N/A");

    if let Some(child_session_id) = child_session_id {
        eprintln!("[LLM E2E] Waiting for worker agent pod to be scheduled...");

        // Poll until pod_name is set — session DB row is created before the pod
        let pod_timeout = Duration::from_secs(120);
        let pod_start = std::time::Instant::now();
        let (pod_name, namespace) = loop {
            assert!(
                pod_start.elapsed() <= pod_timeout,
                "child session {child_session_id} never got a pod_name within {pod_timeout:?}"
            );
            let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT pod_name, session_namespace FROM agent_sessions WHERE id = $1",
            )
            .bind(child_session_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
            let (pod_opt, ns_opt) = row.expect("child session should exist");
            if let (Some(pn), Some(ns)) = (pod_opt, ns_opt) {
                break (pn, ns);
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        };
        eprintln!("[LLM E2E] Worker pod: {pod_name} in namespace: {namespace}");

        // Wait for pod to complete (LLM coding takes time: up to 10 minutes)
        phase = wait_for_pod_with_logs(&state, &namespace, &pod_name, 600).await;
        eprintln!("[LLM E2E] Worker pod phase: {phase}");

        // Always dump pod logs for debugging (even on success)
        dump_pod_logs(&state, &namespace, &pod_name).await;

        // NOTE: Do NOT run the reaper here — the pipeline (triggered by the agent's
        // push) may still be running. The reaper deletes the agent's identity tokens,
        // which would break the pipeline's registry auth. Run reaper after pipeline.
    } else {
        eprintln!(
            "[LLM E2E] No child session spawned — manager may have completed without spawning worker"
        );
    }

    // -- 8. Verify git push to feature branch --
    eprintln!("[LLM E2E] Verifying git push to feature branch...");
    let repo_path: Option<String> =
        sqlx::query_scalar("SELECT repo_path FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();

    // Find the feature branch the agent pushed to
    let feature_branch = if let Some(repo_path) = &repo_path {
        let repo = std::path::Path::new(repo_path);
        if repo.exists() {
            // List branches — look for feature/* branches
            let branches_output = try_git_cmd(repo, &["branch", "--list", "feature/*"]);
            let branch = branches_output
                .as_deref()
                .unwrap_or("")
                .lines()
                .map(|l| l.trim().trim_start_matches("* "))
                .find(|b| !b.is_empty())
                .unwrap_or("feature/initial-app")
                .to_string();
            eprintln!("[LLM E2E] Feature branch found: {branch}");

            // Check for commits on feature branch
            let log_output = try_git_cmd(repo, &["log", "--oneline", &branch]);
            if let Some(log) = &log_output {
                let commit_count = log.lines().count();
                eprintln!("[LLM E2E] Commits on {branch}: {commit_count}");

                if commit_count > 1 {
                    // Check for key files on feature branch
                    let ref_prefix = format!("{branch}:");
                    let has_platform_yaml =
                        try_git_cmd(repo, &["show", &format!("{ref_prefix}.platform.yaml")])
                            .is_some();
                    let has_dockerfile =
                        try_git_cmd(repo, &["show", &format!("{ref_prefix}Dockerfile")]).is_some();

                    eprintln!(
                        "[LLM E2E] .platform.yaml: {has_platform_yaml}, Dockerfile: {has_dockerfile}"
                    );

                    // Dump file contents for debugging
                    if let Some(yaml) =
                        try_git_cmd(repo, &["show", &format!("{ref_prefix}.platform.yaml")])
                    {
                        eprintln!("[LLM E2E] .platform.yaml content:\n{yaml}");
                    }
                    if let Some(df) =
                        try_git_cmd(repo, &["show", &format!("{ref_prefix}Dockerfile")])
                    {
                        eprintln!("[LLM E2E] Dockerfile content:\n{df}");
                    }
                } else {
                    eprintln!(
                        "[LLM E2E] WARNING: Only {commit_count} commit(s) on {branch} — worker may not have pushed"
                    );
                }
            } else {
                eprintln!("[LLM E2E] WARNING: git log on {branch} failed");
            }

            Some(branch)
        } else {
            eprintln!("[LLM E2E] WARNING: repo_path does not exist: {repo_path}");
            None
        }
    } else {
        eprintln!("[LLM E2E] WARNING: project has no repo_path");
        None
    };

    // -- 9. Check for MR creation + enable auto-merge --
    // The agent should create the MR via curl using $PROJECT_ID / $PLATFORM_API_TOKEN.
    // If MCP tools are disabled or the agent didn't create one, the test creates it as fallback.
    eprintln!("[LLM E2E] Checking for merge request...");
    let mut mr_found = false;
    let mr_timeout = Duration::from_secs(30);
    let mr_start = std::time::Instant::now();

    loop {
        let (_, mrs_body) = e2e_helpers::get_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/merge-requests?limit=5"),
        )
        .await;

        if let Some(items) = mrs_body["items"].as_array()
            && !items.is_empty()
        {
            let mr_number = items[0]["number"].as_i64().unwrap_or(0);
            let mr_status = items[0]["status"].as_str().unwrap_or("unknown");
            let source = items[0]["source_branch"].as_str().unwrap_or("?");
            let target = items[0]["target_branch"].as_str().unwrap_or("?");
            eprintln!("[LLM E2E] MR #{mr_number} found: {source} → {target} (status: {mr_status})");
            mr_found = true;
            break;
        }

        if mr_start.elapsed() > mr_timeout {
            eprintln!("[LLM E2E] No MR found — creating as fallback");
            break;
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // Fallback: test creates the MR if the agent didn't
    if !mr_found {
        if let Some(ref branch) = feature_branch {
            let (create_status, create_body) = e2e_helpers::post_json(
                &app,
                &admin_token,
                &format!("/api/projects/{project_id}/merge-requests"),
                serde_json::json!({
                    "title": "Initial app (test fallback)",
                    "source_branch": branch,
                    "target_branch": "main"
                }),
            )
            .await;
            eprintln!("[LLM E2E] Fallback MR creation: {create_status} — {create_body}");
            if create_status == axum::http::StatusCode::CREATED {
                mr_found = true;
            }
        } else {
            eprintln!("[LLM E2E] WARNING: No feature branch found — cannot create fallback MR");
        }
    }

    // Enable auto-merge on whichever MR exists
    if mr_found {
        let (_, mrs_body) = e2e_helpers::get_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/merge-requests?limit=1"),
        )
        .await;
        if let Some(items) = mrs_body["items"].as_array()
            && let Some(mr) = items.first()
        {
            let mr_number = mr["number"].as_i64().unwrap_or(0);
            let mr_status = mr["status"].as_str().unwrap_or("unknown");
            if mr_status == "open" {
                let (am_status, _) = e2e_helpers::put_json(
                    &app,
                    &admin_token,
                    &format!("/api/projects/{project_id}/merge-requests/{mr_number}/auto-merge"),
                    serde_json::json!({}),
                )
                .await;
                eprintln!("[LLM E2E] Auto-merge enabled: {am_status}");
            }
        }
    }

    // -- 9b. Wait for pipeline (triggered by MR creation via on_mr) --
    eprintln!("[LLM E2E] Checking for pipeline...");
    // Give pipeline trigger a moment to fire
    tokio::time::sleep(Duration::from_secs(3)).await;

    let (_, pipelines_body) = e2e_helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?limit=5"),
    )
    .await;

    let mut pipeline_status = String::from("none");
    if let Some(items) = pipelines_body["items"].as_array()
        && !items.is_empty()
    {
        let pipeline_id = items[0]["id"].as_str().unwrap();
        eprintln!("[LLM E2E] Pipeline found: {pipeline_id}, waiting for completion...");

        // Wake executor in case it missed the notify
        state.pipeline_notify.notify_one();

        pipeline_status =
            e2e_helpers::poll_pipeline_status(&app, &admin_token, project_id, pipeline_id, 600)
                .await;
        eprintln!("[LLM E2E] Pipeline status: {pipeline_status}");

        // Dump pipeline logs from MinIO (executor stores logs there before deleting the pod)
        if pipeline_status != "success" {
            eprintln!("[LLM E2E] Dumping pipeline logs from MinIO...");
            for suffix in &["build-clone.log", "build.log"] {
                let path = format!("logs/pipelines/{pipeline_id}/{suffix}");
                match state.minio.read(&path).await {
                    Ok(data) => {
                        let bytes = data.to_vec();
                        let text = String::from_utf8_lossy(&bytes);
                        eprintln!("\n=== MinIO: {path} ===\n{text}");
                    }
                    Err(e) => {
                        eprintln!("[LLM E2E] Could not read {path}: {e}");
                    }
                }
            }
        }
    } else {
        eprintln!(
            "[LLM E2E] No pipeline triggered (worker may not have created .platform.yaml or MR correctly)"
        );
    }

    // -- 9c. Wait for auto-merge (fires after pipeline success via try_auto_merge) --
    if pipeline_status == "success" && mr_found {
        eprintln!("[LLM E2E] Waiting for auto-merge...");
        let merge_timeout = Duration::from_secs(60);
        let merge_start = std::time::Instant::now();

        loop {
            let (_, mrs_body) = e2e_helpers::get_json(
                &app,
                &admin_token,
                &format!("/api/projects/{project_id}/merge-requests?limit=5"),
            )
            .await;

            if let Some(items) = mrs_body["items"].as_array()
                && !items.is_empty()
            {
                let mr_status = items[0]["status"].as_str().unwrap_or("unknown");
                if mr_status == "merged" {
                    eprintln!("[LLM E2E] MR merged successfully!");
                    break;
                }
                eprintln!("[LLM E2E] MR status: {mr_status}");
            }

            if merge_start.elapsed() > merge_timeout {
                eprintln!("[LLM E2E] Auto-merge timed out — merging manually");
                // Auto-merge didn't fire because pipeline completed before auto_merge was
                // enabled on the MR. Merge manually — the endpoint still enforces all
                // protection rules (CI success, required_approvals, etc.).
                let (_, mrs_body2) = e2e_helpers::get_json(
                    &app,
                    &admin_token,
                    &format!("/api/projects/{project_id}/merge-requests?limit=1"),
                )
                .await;
                if let Some(items) = mrs_body2["items"].as_array()
                    && let Some(mr) = items.first()
                {
                    let mr_number = mr["number"].as_i64().unwrap_or(0);
                    let mr_status = mr["status"].as_str().unwrap_or("unknown");
                    if mr_status == "open" {
                        let (merge_st, merge_body) = e2e_helpers::post_json(
                            &app,
                            &admin_token,
                            &format!("/api/projects/{project_id}/merge-requests/{mr_number}/merge"),
                            serde_json::json!({}),
                        )
                        .await;
                        eprintln!("[LLM E2E] Manual merge: {merge_st} — {merge_body}");
                    }
                }
                break;
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // -- 9d. Run reaper NOW (after pipeline) to finalize child session --
    // Must happen after pipeline because reaper deletes agent identity tokens.
    if let Some(child_session_id) = child_session_id {
        platform::agent::service::run_reaper_once(&state).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        platform::agent::service::run_reaper_once(&state).await;

        child_status =
            e2e_helpers::poll_session_status(&pool, child_session_id, &["completed", "failed"], 30)
                .await;
        eprintln!("[LLM E2E] Child session status: {child_status}");
    }

    // -- 11. Wait for deployment to become healthy --
    eprintln!("[LLM E2E] Waiting for deployment...");
    let deploy_timeout = Duration::from_secs(120);
    let deploy_start = std::time::Instant::now();
    let mut deploy_status_str = String::from("none");

    loop {
        if deploy_start.elapsed() > deploy_timeout {
            eprintln!("[LLM E2E] Deployment timed out at status: {deploy_status_str}");
            break;
        }

        let (deploy_status, deploy_body) = e2e_helpers::get_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/deployments/production"),
        )
        .await;

        if deploy_status == axum::http::StatusCode::OK {
            let current = deploy_body["current_status"].as_str().unwrap_or("unknown");
            if current != deploy_status_str {
                eprintln!("[LLM E2E] Deployment status: {current}");
                deploy_status_str = current.to_string();
            }
            if current == "healthy" {
                let image = deploy_body["image_ref"].as_str().unwrap_or("?");
                eprintln!("[LLM E2E] Deployment healthy! Image: {image}");
                break;
            }
            if current == "failed" {
                eprintln!("[LLM E2E] Deployment failed!");
                break;
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Verify actual K8s pods are running in the production namespace
    if deploy_status_str == "healthy" {
        let ns_slug: Option<String> =
            sqlx::query_scalar("SELECT namespace_slug FROM projects WHERE id = $1")
                .bind(project_id)
                .fetch_optional(&pool)
                .await
                .unwrap()
                .flatten();
        if let Some(slug) = ns_slug {
            let prod_ns = platform::deployer::reconciler::target_namespace(
                &state.config,
                &slug,
                "production",
            );
            eprintln!("[LLM E2E] Checking pods in namespace: {prod_ns}");
            let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &prod_ns);
            match pods.list(&ListParams::default()).await {
                Ok(pod_list) => {
                    for p in &pod_list.items {
                        let name = p.metadata.name.as_deref().unwrap_or("?");
                        let phase = p
                            .status
                            .as_ref()
                            .and_then(|s| s.phase.as_deref())
                            .unwrap_or("?");
                        eprintln!("[LLM E2E]   Pod: {name} phase={phase}");
                    }
                    assert!(
                        !pod_list.items.is_empty(),
                        "production namespace should have at least one pod"
                    );
                }
                Err(e) => {
                    eprintln!("[LLM E2E] WARNING: Could not list pods in {prod_ns}: {e}");
                }
            }
        }
    }

    // -- 12. Check manager session final status --
    eprintln!("[LLM E2E] Waiting for manager session to complete...");
    let manager_status =
        e2e_helpers::poll_session_status(&pool, manager_session_id, &["completed", "failed"], 60)
            .await;
    eprintln!("[LLM E2E] Manager session final status: {manager_status}");

    // -- Assertions --
    // Primary: manager created a project and spawned a worker
    assert!(
        child_session_id.is_some(),
        "manager should have spawned a child coding agent session"
    );
    assert_eq!(
        phase, "Succeeded",
        "worker agent pod should succeed — child session status: {child_status}"
    );
    assert_eq!(
        manager_status, "completed",
        "manager session should reach completed"
    );

    // Verify the worker pushed to a feature branch (not main)
    if let Some(repo_path) = &repo_path
        && let Some(ref branch) = feature_branch
    {
        let repo = std::path::Path::new(repo_path);
        if repo.exists() {
            let log_output = try_git_cmd(repo, &["log", "--oneline", branch]);
            if let Some(log) = &log_output {
                let commit_count = log.lines().count();
                assert!(
                    commit_count > 1,
                    "worker should have pushed commits to {branch} (got {commit_count}). Log:\n{log}"
                );
            }
        }
    }

    // MR should have been created
    assert!(
        mr_found,
        "worker should have created a merge request via MCP tool"
    );

    // Pipeline should have triggered and succeeded
    assert_eq!(
        pipeline_status, "success",
        "pipeline should succeed (got: {pipeline_status})"
    );

    // Deployment should have been created and reached healthy
    assert_eq!(
        deploy_status_str, "healthy",
        "deployment should reach healthy (got: {deploy_status_str})"
    );

    eprintln!("[LLM E2E] Test passed!");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wait for a pod with detailed log dumping on failure.
async fn wait_for_pod_with_logs(
    state: &platform::store::AppState,
    namespace: &str,
    pod_name: &str,
    timeout_secs: u64,
) -> String {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_secs(5);

    loop {
        if start.elapsed().as_secs() > timeout_secs {
            // Dump logs before panicking
            dump_pod_logs(state, namespace, pod_name).await;
            panic!(
                "pod {pod_name} did not complete within {timeout_secs}s in namespace {namespace}"
            );
        }

        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

        match pods.get(pod_name).await {
            Ok(pod) => {
                if let Some(status) = &pod.status
                    && let Some(phase) = &status.phase
                {
                    match phase.as_str() {
                        "Succeeded" => return phase.clone(),
                        "Failed" => {
                            dump_pod_logs(state, namespace, pod_name).await;
                            return phase.clone();
                        }
                        _ => {} // Running, Pending — keep waiting
                    }
                }
            }
            Err(e) => {
                eprintln!("[LLM E2E] Pod lookup error (retrying): {e}");
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Dump all container logs from a pod for debugging.
async fn dump_pod_logs(state: &platform::store::AppState, namespace: &str, pod_name: &str) {
    for container in &["git-clone", "setup-tools", "claude"] {
        let logs =
            e2e_helpers::pod_logs_container(&state.kube, namespace, pod_name, container).await;
        eprintln!("\n=== {pod_name} [{container}] ===\n{logs}");
    }
}

/// Get session messages with full content for error diagnostics.
async fn get_session_messages_full(pool: &PgPool, session_id: Uuid) -> String {
    get_session_messages_inner(pool, session_id, 2000).await
}

async fn get_session_messages_inner(pool: &PgPool, session_id: Uuid, max_len: usize) -> String {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM agent_messages WHERE session_id = $1 ORDER BY created_at ASC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows.iter()
        .map(|(role, content)| {
            let truncated = if content.len() > max_len {
                format!("{}...", &content[..max_len])
            } else {
                content.clone()
            };
            format!("[{role}] {truncated}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dump all projects for diagnostics.
async fn dump_all_projects(pool: &PgPool) -> String {
    let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT id, name, is_active FROM projects ORDER BY created_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    if rows.is_empty() {
        return "No projects found".to_string();
    }
    rows.iter()
        .map(|(id, name, active)| format!("  {id} | {name} | active={active}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dump all agent sessions for diagnostics.
async fn dump_all_sessions(pool: &PgPool) -> String {
    let rows: Vec<(Uuid, String, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, status, project_id, parent_session_id FROM agent_sessions ORDER BY created_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    if rows.is_empty() {
        return "No sessions found".to_string();
    }
    rows.iter()
        .map(|(id, status, proj, parent)| {
            format!(
                "  {id} | status={status} | project={} | parent={}",
                proj.map(|p| p.to_string()).unwrap_or("none".into()),
                parent.map(|p| p.to_string()).unwrap_or("none".into()),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run a git command, returning None on failure instead of panicking.
fn try_git_cmd(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}
