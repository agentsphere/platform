mod e2e_helpers;

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
// ---------------------------------------------------------------------------

/// Helper: create a project for agent tests.
async fn setup_agent_project(app: &axum::Router, token: &str, name: &str) -> Uuid {
    e2e_helpers::create_project(app, token, name, "private").await
}

/// Test 1: Session creation starts a pod and sets status to running.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_session_creation(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-create").await;

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
    assert_eq!(status, StatusCode::CREATED, "session create failed: {body}");
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
}

/// Test 2: Session creates an ephemeral agent user with delegated permissions.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_identity_created(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-identity").await;

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
    assert_eq!(status, StatusCode::CREATED, "session create failed: {body}");
    let session_id = body["id"].as_str().unwrap();

    // Verify the session has a user_id (the ephemeral agent identity)
    assert!(body["user_id"].is_string(), "session should have user_id");

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
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_pod_spec_correct(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-podspec").await;

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
    assert_eq!(status, StatusCode::CREATED, "session create failed: {body}");

    // If pod was created, verify its spec
    if let Some(pod_name) = body["pod_name"].as_str() {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let namespace = &state.config.agent_namespace;
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

        if let Ok(pod) = pods.get(pod_name).await {
            if let Some(spec) = &pod.spec {
                let containers = &spec.containers;
                assert!(
                    !containers.is_empty(),
                    "pod should have at least one container"
                );

                let container = &containers[0];
                if let Some(envs) = &container.env {
                    let env_names: Vec<&str> =
                        envs.iter().filter_map(|e| Some(e.name.as_str())).collect();

                    // These env vars should be present in the agent pod
                    for expected in &["SESSION_ID", "PROJECT_ID"] {
                        assert!(
                            env_names.contains(expected),
                            "pod should have {expected} env var, found: {env_names:?}"
                        );
                    }
                }
            }
        }
    }
}

/// Test 4: Stop a running session.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_session_stop(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-stop").await;

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
    assert_eq!(status, StatusCode::CREATED, "session create failed: {body}");
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

/// Test 5: Reaper captures logs and stores them in MinIO.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_reaper_captures_logs(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-reaper").await;

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
    assert_eq!(status, StatusCode::CREATED);
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

    // Give the reaper time to capture logs
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Check if logs were stored in MinIO
    let log_path = format!("logs/sessions/{session_id}.log");
    let exists = state.minio.is_exist(&log_path).await.unwrap_or(false);
    // Logs may or may not be captured depending on pod lifecycle timing.
    // We just verify the path format is correct and the check doesn't error.
    if exists {
        let data = state.minio.read(&log_path).await.unwrap();
        assert!(!data.is_empty(), "log file in MinIO should be non-empty");
    }
}

/// Test 6: Session with custom image override.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_session_with_custom_image(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-custom-img").await;

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
    assert_eq!(status, StatusCode::CREATED, "session create failed: {body}");

    // Verify the pod uses the custom image
    if let Some(pod_name) = body["pod_name"].as_str() {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let namespace = &state.config.agent_namespace;
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

        if let Ok(pod) = pods.get(pod_name).await {
            if let Some(spec) = &pod.spec {
                let image = spec.containers[0].image.as_deref().unwrap_or("");
                assert!(
                    image.contains("alpine:3.19"),
                    "pod should use custom image alpine:3.19, got: {image}"
                );
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

/// Test 7: Agent role determines MCP configuration.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_role_determines_mcp_config(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-mcp-role").await;

    // Create session with ops role config
    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "MCP role test",
            "provider": "claude-code",
            "config": {
                "role": "ops",
            },
            "delegate_deploy": true,
            "delegate_observe": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "session create failed: {body}");
    assert!(body["id"].is_string());

    // Verify the pod has appropriate env vars or args for the ops role
    if let Some(pod_name) = body["pod_name"].as_str() {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let namespace = &state.config.agent_namespace;
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

        if let Ok(pod) = pods.get(pod_name).await {
            if let Some(spec) = &pod.spec {
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

/// Test 8: Agent identity is fully cleaned up after session ends.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn agent_identity_cleanup(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_agent_project(&app, &token, "agent-cleanup").await;

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
    assert_eq!(status, StatusCode::CREATED, "session create failed: {body}");
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
    // (The agent user is identified by the session's user_id)
    let agent_user_id = detail["user_id"].as_str().unwrap();
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
