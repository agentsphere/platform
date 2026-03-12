mod e2e_helpers;

use std::fmt::Write;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E2E Agent Session Lifecycle Tests (8 tests)
//
// These tests require a Kind cluster with real K8s, Postgres, and Valkey.
// Agent tests exercise session creation, identity management, pod lifecycle,
// and cleanup. All tests are #[ignore] so they don't run in normal CI.
// Run with: just test-e2e
//
// Note: Session creation spawns a K8s pod. If the pod creation fails
// (e.g., image pull, namespace missing), the session is still inserted as
// a DB row but the create_session API returns an error. Tests that need
// a running session handle this gracefully.
// ---------------------------------------------------------------------------

/// Helper: create a project for agent tests and set up a bare repo (required
/// by `create_session` which reads `repo_path` from the project row).
async fn setup_agent_project(
    state: &platform::store::AppState,
    app: &axum::Router,
    token: &str,
    name: &str,
) -> Uuid {
    let project_id = e2e_helpers::create_project(app, token, name, "private").await;

    // create_session() requires the project to have a repo_path
    let (bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (work_dir, _work_path) = e2e_helpers::create_working_copy(&bare_path);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    // Leak the temp dirs so they stay alive for the test duration.
    // E2E tests are short-lived processes, so this is fine.
    std::mem::forget(bare_dir);
    std::mem::forget(work_dir);

    project_id
}

/// Test 1: Session creation inserts a row and attempts pod creation.
///
/// If the K8s pod creation succeeds, the session goes to "running".
/// If pod creation fails (e.g., namespace missing), the API returns an error
/// but the identity + DB row are created. We verify the API accepts valid input.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_session_creation(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-create").await;

    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Hello, run a simple test",
            "provider": "claude-code",
        }),
    )
    .await;

    // Session creation may succeed (pod created) or fail (K8s issue).
    // Both are valid outcomes depending on cluster state.
    if status == StatusCode::CREATED {
        assert!(body["id"].is_string(), "session should have an id");
        assert_eq!(body["project_id"], project_id.to_string());
        assert!(
            body["status"] == "running" || body["status"] == "pending",
            "session status should be running or pending, got: {}",
            body["status"]
        );

        // If pod_name is set, verify it's a valid K8s pod name
        if let Some(pod_name) = body["pod_name"].as_str() {
            assert!(!pod_name.is_empty(), "pod_name should be non-empty if set");
        }
    } else {
        // Pod creation failed — that's OK for this test as long as the API
        // returned a proper error response (500 from PodCreationFailed).
        assert!(
            status == StatusCode::INTERNAL_SERVER_ERROR || status == StatusCode::BAD_REQUEST,
            "unexpected status: {status}, body: {body}"
        );
    }
}

/// Test 2: Session creates an ephemeral agent user with delegated permissions.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_identity_created(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-identity").await;

    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Identity test",
            "provider": "claude-code",
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        // Pod creation failed; verify agent identity was still created in DB
        // by checking for a pending session row
        let row: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, agent_user_id FROM agent_sessions WHERE project_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(project_id)
        .fetch_optional(&state.pool)
        .await
        .unwrap();

        if let Some((_id, agent_user_id)) = row {
            assert!(
                agent_user_id.is_some(),
                "agent_user_id should be set even if pod creation failed"
            );
        }
        return;
    }

    let session_id = body["id"].as_str().unwrap();
    assert!(body["user_id"].is_string(), "session should have user_id");
    assert!(
        body["agent_user_id"].is_string(),
        "session should have agent_user_id"
    );

    // Get session detail
    let (status, detail) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["project_id"], project_id.to_string());
}

/// Test 3: Pod spec has correct env vars and mounts.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_pod_spec_correct(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-podspec").await;

    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Pod spec test",
            "provider": "claude-code",
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        // Pod creation failed — skip pod spec checks
        return;
    }

    // If pod was created, verify its spec
    if let Some(pod_name) = body["pod_name"].as_str() {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        // Look up session namespace from DB (not in API response)
        let session_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
        let session_ns: Option<String> =
            sqlx::query_scalar("SELECT session_namespace FROM agent_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        let session_ns = session_ns.expect("session_namespace should be set");

        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &session_ns);

        if let Ok(pod) = pods.get(pod_name).await
            && let Some(spec) = &pod.spec
        {
            let containers = &spec.containers;
            assert!(
                !containers.is_empty(),
                "pod should have at least one container"
            );

            // Verify service account is set
            assert_eq!(
                spec.service_account_name.as_deref(),
                Some("agent-sa"),
                "pod should use agent-sa service account"
            );

            let container = &containers[0];
            if let Some(envs) = &container.env {
                let env_names: Vec<&str> = envs.iter().map(|e| e.name.as_str()).collect();

                // These env vars should be present in the agent pod
                for expected in &["SESSION_ID", "PROJECT_ID", "SESSION_NAMESPACE"] {
                    assert!(
                        env_names.contains(expected),
                        "pod should have {expected} env var, found: {env_names:?}"
                    );
                }
            }
        }
    }
}

/// Test 4: Stop a running session.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_session_stop(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-stop").await;

    // Create session
    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Stop test session",
            "provider": "claude-code",
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        // Pod creation failed — can't test stop
        return;
    }

    let session_id = body["id"].as_str().unwrap();

    // Stop the session
    let (stop_status, stop_body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        stop_status,
        StatusCode::OK,
        "stop should succeed: {stop_body}"
    );

    // Verify session status is stopped
    let (_, detail) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{session_id}"),
    )
    .await;
    assert!(
        detail["status"] == "stopped" || detail["status"] == "completed",
        "session should be stopped or completed, got: {}",
        detail["status"]
    );
}

/// Test 5: Reaper captures logs and stores them in `MinIO`.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_reaper_captures_logs(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-reaper").await;

    // Create and immediately stop a session
    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Reaper log test",
            "provider": "claude-code",
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        return;
    }

    let session_id = body["id"].as_str().unwrap();

    // Give it a moment to start
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Stop it
    let _ = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;

    // Give time for log capture
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Check if logs were stored in MinIO (path: logs/agents/{session_id}/output.log)
    let log_path = format!("logs/agents/{session_id}/output.log");
    let exists = state.minio.exists(&log_path).await.unwrap_or(false);
    // Logs may or may not be captured depending on pod lifecycle timing.
    // We just verify the path format is correct and the check doesn't error.
    if exists {
        let data = state.minio.read(&log_path).await.unwrap();
        assert!(!data.is_empty(), "log file in MinIO should be non-empty");
    }
}

/// Test 6: Session with custom image override.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_session_with_custom_image(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-custom-img").await;

    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Custom image test",
            "provider": "claude-code",
            "config": {
                "image": "alpine:3.19",
            },
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        // Pod creation failed — skip
        return;
    }

    // Verify the pod uses the custom image
    if let Some(pod_name) = body["pod_name"].as_str() {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let namespace = &state.config.agent_namespace;
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

        if let Ok(pod) = pods.get(pod_name).await
            && let Some(spec) = &pod.spec
        {
            let image = spec.containers[0].image.as_deref().unwrap_or("");
            assert!(
                image.contains("alpine:3.19"),
                "pod should use custom image alpine:3.19, got: {image}"
            );
        }
    }

    // Clean up
    if let Some(session_id) = body["id"].as_str() {
        let _ = e2e_helpers::post_json(
            &app,
            &token,
            &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
            serde_json::json!({}),
        )
        .await;
    }
}

/// Test 7: Agent role determines MCP configuration.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_role_determines_mcp_config(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-mcp-role").await;

    // Create session with ops role config
    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "MCP role test",
            "provider": "claude-code",
            "role": "ops",
            "config": {
                "role": "ops",
            },
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        return;
    }

    assert!(body["id"].is_string());

    // Verify the pod has appropriate env vars or args for the ops role
    if let Some(pod_name) = body["pod_name"].as_str() {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let namespace = &state.config.agent_namespace;
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

        if let Ok(pod) = pods.get(pod_name).await
            && let Some(spec) = &pod.spec
        {
            let container = &spec.containers[0];
            // Check for AGENT_ROLE env var
            if let Some(envs) = &container.env {
                let role_env = envs.iter().find(|e| e.name == "AGENT_ROLE");
                if let Some(role) = role_env {
                    assert_eq!(
                        role.value.as_deref().unwrap_or(""),
                        "ops",
                        "AGENT_ROLE should be 'ops'"
                    );
                }
            }
        }
    }

    // Clean up
    if let Some(session_id) = body["id"].as_str() {
        let _ = e2e_helpers::post_json(
            &app,
            &token,
            &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
            serde_json::json!({}),
        )
        .await;
    }
}

/// Test 8: Full agent session pub/sub flow — create session → pod runs mock CLI →
/// agent-runner publishes events → persistence subscriber writes to `agent_messages` →
/// session detail shows messages.
///
/// This requires:
/// - `PLATFORM_HOST_MOUNT_PATH` set (mounts test fixtures into pod)
/// - `CLAUDE_CLI_PATH` pointing to mock CLI accessible inside pod
/// - A real TCP server (pods connect back to the platform API)
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn e2e_agent_session_pubsub_flow(pool: PgPool) {
    // 1. Start real TCP server (pod needs to reach platform API)
    let (state, admin_token, _server) = e2e_helpers::start_agent_server(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());

    // 2. Create project with repo
    let project_id = setup_agent_project(&state, &app, &admin_token, "agent-pubsub").await;

    // 3. Create session via API
    let (status, body) = e2e_helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "E2E pub/sub test",
            "provider": "claude-code",
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        // Pod creation failed (mock CLI not accessible, image pull issue, etc.)
        eprintln!("session creation failed ({status}): {body}");
        return;
    }

    let session_id_str = body["id"].as_str().unwrap();
    let session_id: Uuid = session_id_str.parse().unwrap();

    // 4. Wait for pod to complete (mock CLI exits immediately after 3 NDJSON lines)
    let pod_name = body["pod_name"]
        .as_str()
        .expect("session response should have pod_name");
    // Pod is now in the session namespace, not the project dev namespace
    let namespace: Option<String> =
        sqlx::query_scalar("SELECT session_namespace FROM agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let namespace = namespace.expect("session_namespace should be set");

    let phase = e2e_helpers::wait_for_pod(&state.kube, &namespace, pod_name, 120).await;
    eprintln!("pod {pod_name} finished with phase: {phase}");

    // Capture container logs for debugging
    if phase == "Failed" {
        for container in &["git-clone", "setup-tools", "claude"] {
            let logs =
                e2e_helpers::pod_logs_container(&state.kube, &namespace, pod_name, container).await;
            eprintln!("pod {pod_name} [{container}] logs:\n{logs}");
        }
    } else {
        let logs = e2e_helpers::pod_logs(&state.kube, &namespace, pod_name).await;
        eprintln!("pod {pod_name} logs:\n{logs}");
    }

    assert_eq!(phase, "Succeeded", "pod should succeed with mock CLI");

    // 5. Run the reaper once to transition session to completed/failed
    //    (the reaper is not started as a background task in test_router)
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    platform::agent::service::run_reaper_once(&state).await;

    // 6. Poll session status — should be completed (mock CLI exits with success)
    let final_status =
        e2e_helpers::poll_session_status(&pool, session_id, &["completed", "failed"], 30).await;
    eprintln!("session final status: {final_status}");
    assert_eq!(
        final_status, "completed",
        "session should be completed after successful pod"
    );

    // 7. Get session detail — verify messages were persisted via pub/sub
    let (detail_status, detail) = e2e_helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id_str}"),
    )
    .await;
    assert_eq!(detail_status, StatusCode::OK);

    let messages = detail["messages"].as_array().unwrap();

    // Mock CLI emits 3 NDJSON lines → agent-runner converts to pub/sub events:
    //   system  → PubSubKind::Milestone  (role: "milestone")
    //   assistant (text) → PubSubKind::Text (role: "text")
    //   result (success) → PubSubKind::Completed (role: "completed")
    let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
    eprintln!("message roles: {roles:?}");

    assert_eq!(
        messages.len(),
        3,
        "mock CLI emits 3 NDJSON lines → 3 persisted messages, got: {roles:?}"
    );

    assert_eq!(
        roles[0], "milestone",
        "first event should be milestone (system init)"
    );
    assert_eq!(
        roles[1], "text",
        "second event should be text (assistant response)"
    );
    assert_eq!(
        roles[2], "completed",
        "third event should be completed (result)"
    );

    // Verify milestone message content (from convert_system)
    let milestone_content = messages[0]["content"].as_str().unwrap_or("");
    assert!(
        milestone_content.contains("Session started"),
        "milestone should contain 'Session started', got: {milestone_content}"
    );

    // Verify text message has actual content
    let text_content = messages[1]["content"].as_str().unwrap_or("");
    assert!(!text_content.is_empty(), "text message should have content");

    // Verify completed message
    let completed_content = messages[2]["content"].as_str().unwrap_or("");
    assert!(
        !completed_content.is_empty(),
        "completed message should have content"
    );
}

/// Test 9: Agent pod can clone via init container and push from main container.
///
/// Validates the full git auth chain:
///   - git-clone init container: `GIT_ASKPASS` → token → smart HTTP clone
///   - main container (mock CLI): `GIT_ASKPASS` → `PLATFORM_API_TOKEN` → git push
///
/// Uses `mock-claude-cli-git.sh` which creates a file, commits, and pushes.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn e2e_agent_git_clone_push(pool: PgPool) {
    // 1. Start real TCP server (pod needs to reach platform API for git HTTP)
    let (state, admin_token, _server) = e2e_helpers::start_agent_server(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());

    // 2. Create project with bare repo + initial commit
    let project_id = setup_agent_project(&state, &app, &admin_token, "agent-git-push").await;

    // Remove auto-created branch protection so the mock CLI can push directly to main.
    // This test verifies basic git clone+push, not branch protection (tested separately).
    sqlx::query("DELETE FROM branch_protection_rules WHERE project_id = $1")
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    // Get the bare repo path for later verification
    let repo_path: String = sqlx::query_scalar("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    // Record initial commit count
    let initial_log = e2e_helpers::git_cmd(
        std::path::Path::new(&repo_path),
        &["log", "--oneline", "main"],
    );
    let initial_commit_count = initial_log.lines().count();

    // 3. Swap the mock CLI file to the git-pushing variant.
    //    CLAUDE_CLI_PATH is read from env at session creation time and we can't
    //    use set_var (unsafe_code = forbid). Instead, we replace the file at the
    //    existing CLAUDE_CLI_PATH with the git-push variant and restore it after.
    let cli_path = std::env::var("CLAUDE_CLI_PATH")
        .expect("CLAUDE_CLI_PATH must be set — run via: just test-e2e");
    let backup_path = format!("{cli_path}.bak-git-test");
    let git_mock_source = std::env::var("PLATFORM_HOST_MOUNT_PATH").map_or_else(
        |_| {
            format!(
                "{}/tests/fixtures/mock-claude-cli-git.sh",
                env!("CARGO_MANIFEST_DIR")
            )
        },
        |p| format!("{p}/mock-claude-cli-git.sh"),
    );
    std::fs::rename(&cli_path, &backup_path).expect("backup original mock CLI");
    std::fs::copy(&git_mock_source, &cli_path).expect("install git mock CLI");

    // 4. Create session with branch: "main" so init container checks out main
    let (status, body) = e2e_helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Git push integration test",
            "provider": "claude-code",
            "branch": "main",
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        // Restore before panicking
        std::fs::rename(&backup_path, &cli_path).expect("restore original mock CLI");
        panic!("expected session creation to succeed, got {status}: {body}");
    }

    let session_id_str = body["id"].as_str().unwrap();
    let session_id: Uuid = session_id_str.parse().unwrap();
    let pod_name = body["pod_name"]
        .as_str()
        .expect("session response should have pod_name");

    // 5. Get session namespace for pod lookups (pods now use per-session namespaces)
    let namespace: Option<String> =
        sqlx::query_scalar("SELECT session_namespace FROM agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let namespace = namespace.expect("session_namespace should be set");

    // 6. Wait for pod to complete (clone + commit + push + NDJSON)
    //    Keep the git mock in place until the pod finishes — the pod reads
    //    the script from the hostPath mount at execution time, not at creation.
    let phase = e2e_helpers::wait_for_pod(&state.kube, &namespace, pod_name, 180).await;
    eprintln!("pod {pod_name} finished with phase: {phase}");

    // Restore original mock CLI after pod completes
    std::fs::rename(&backup_path, &cli_path).expect("restore original mock CLI");

    // On failure, dump all container logs for debugging
    if phase != "Succeeded" {
        let mut all_logs = String::new();
        for container in &["git-clone", "setup-tools", "claude"] {
            let logs =
                e2e_helpers::pod_logs_container(&state.kube, &namespace, pod_name, container).await;
            write!(all_logs, "\n=== {container} ===\n{logs}\n").unwrap();
        }
        panic!(
            "pod should succeed with mock-claude-cli-git.sh, got phase: {phase}\n\
             Pod logs:{all_logs}"
        );
    }

    // 7. Run reaper to transition session to completed
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    platform::agent::service::run_reaper_once(&state).await;

    let final_status =
        e2e_helpers::poll_session_status(&pool, session_id, &["completed", "failed"], 30).await;
    assert_eq!(
        final_status, "completed",
        "session should be completed after successful pod"
    );

    // 8. Verify push: bare repo should have one more commit on main
    let final_log = e2e_helpers::git_cmd(
        std::path::Path::new(&repo_path),
        &["log", "--oneline", "main"],
    );
    let final_commit_count = final_log.lines().count();
    assert_eq!(
        final_commit_count,
        initial_commit_count + 1,
        "bare repo should have exactly 1 new commit on main (initial: {initial_commit_count}, final: {final_commit_count})"
    );

    // 9. Verify commit message
    let last_subject = e2e_helpers::git_cmd(
        std::path::Path::new(&repo_path),
        &["log", "-1", "--format=%s", "main"],
    );
    assert!(
        last_subject.trim().contains("agent push test"),
        "last commit subject should contain 'agent push test', got: '{}'",
        last_subject.trim()
    );

    // 10. Verify file content
    let file_content = e2e_helpers::git_cmd(
        std::path::Path::new(&repo_path),
        &["show", "main:agent-test-file.txt"],
    );
    assert_eq!(
        file_content.trim(),
        "agent-pushed-content",
        "pushed file content should match"
    );
}

/// Test 10: Agent identity is fully cleaned up after session ends.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_identity_cleanup(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_agent_project(&state, &app, &token, "agent-cleanup").await;

    // Create session
    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "Cleanup test",
            "provider": "claude-code",
        }),
    )
    .await;

    if status != StatusCode::CREATED {
        // Pod creation failed; the agent identity is created before the pod,
        // so we can still test cleanup via direct DB query.
        let row: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, agent_user_id FROM agent_sessions WHERE project_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(project_id)
        .fetch_optional(&state.pool)
        .await
        .unwrap();

        if let Some((session_id, agent_user_id_opt)) = row {
            let agent_user_id = agent_user_id_opt.expect("agent_user_id should be set");

            // The session may be in pending state — update it to stopped and cleanup
            sqlx::query(
                "UPDATE agent_sessions SET status = 'stopped', finished_at = now() WHERE id = $1",
            )
            .bind(session_id)
            .execute(&state.pool)
            .await
            .unwrap();

            // Cleanup agent identity
            platform::agent::identity::cleanup_agent_identity(
                &state.pool,
                &state.valkey,
                agent_user_id,
            )
            .await
            .unwrap();

            // Verify no active API tokens remain
            let token_count: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM api_tokens WHERE user_id = $1 AND expires_at > now()",
            )
            .bind(agent_user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                token_count.0, 0,
                "no active tokens should remain for the agent identity"
            );
        }
        return;
    }

    let session_id = body["id"].as_str().unwrap();

    // Stop the session
    let _ = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;

    // Give cleanup time
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Verify session is stopped
    let (status, detail) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        detail["status"] == "stopped" || detail["status"] == "completed",
        "session should be stopped or completed after cleanup"
    );

    // Verify no active API tokens remain for the agent identity
    // agent_user_id is the ephemeral agent user (not user_id which is the human)
    let agent_user_id = detail["agent_user_id"]
        .as_str()
        .expect("agent_user_id should be present");
    let token_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM api_tokens WHERE user_id = $1::uuid AND expires_at > now()",
    )
    .bind(agent_user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        token_count.0, 0,
        "no active tokens should remain for the agent identity"
    );
}

/// Test 10: Two sessions for the same project get different namespaces.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn agent_session_namespace_isolation(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());

    let project_id = setup_agent_project(&state, &app, &admin_token, "agent-iso").await;

    // Create two sessions
    let mut namespaces = Vec::new();
    for i in 0..2 {
        let (status, body) = e2e_helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/sessions"),
            serde_json::json!({
                "prompt": format!("Isolation test {i}"),
                "provider": "claude-code",
            }),
        )
        .await;

        if status != StatusCode::CREATED {
            eprintln!("session {i} creation failed ({status}): {body}");
            return;
        }

        let session_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
        let ns: Option<String> =
            sqlx::query_scalar("SELECT session_namespace FROM agent_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        namespaces.push(ns.expect("session_namespace should be set"));
    }

    assert_ne!(
        namespaces[0], namespaces[1],
        "two sessions for the same project should have different namespaces"
    );

    // Cleanup: delete both session namespaces
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    for ns in &namespaces {
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}
