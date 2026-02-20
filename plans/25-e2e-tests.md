# Plan 25 — E2E Test Suite

## Overview

End-to-end tests that validate the full platform stack: git operations, pipeline execution with real K8s pods, webhook dispatch with HMAC verification, WebSocket streaming, and deployer reconciliation. These tests run against a Kind cluster and exercise code paths that integration tests (Plan 17) cannot reach.

**Target: ~40 E2E tests across 5 test files + shared infrastructure.**

---

## Motivation

- **Integration tests cover API handlers** but not git operations, K8s pod lifecycle, or real I/O
- **Pipeline executor** creates real K8s pods — untestable without a cluster
- **Deployer reconciler** applies K8s manifests — untestable without a cluster
- **Webhook dispatch** makes outbound HTTP calls with HMAC signing — needs a mock server
- **Agent session lifecycle** involves pod creation, log streaming, reaping — full-stack only
- **Git operations** (smart HTTP push/pull, merge, LFS) need real repos with commits

---

## Prerequisites

| Requirement | How to Provide | Used By |
|---|---|---|
| Kind cluster | `just cluster-up` | All E2E tests |
| PostgreSQL | Via Kind (port-forwarded) | All tests |
| Valkey | Via Kind (port-forwarded) | All tests |
| MinIO | Via Kind (port-forwarded) | Log/artifact storage |
| Platform binary | `just build` | Git push/pull tests |
| Docker | For building test images | Pipeline tests |
| `wiremock` crate | Dev dependency | Webhook tests |

### New Dev Dependencies

```toml
# Cargo.toml [dev-dependencies]
wiremock = "0.6"              # Mock HTTP server for webhook verification
tempfile = "3"                # Temporary directories for git repos
```

---

## Architecture

### Test Environment

```
┌──────────────────────────────────────────────────┐
│ Kind Cluster                                      │
│ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐│
│ │Postgres │ │ Valkey  │ │  MinIO  │ │ Platform││
│ │:5432    │ │ :6379   │ │ :9000   │ │ :8080   ││
│ └─────────┘ └─────────┘ └─────────┘ └─────────┘│
│                                                   │
│ ┌──────────────────────────────────────────────┐ │
│ │ Test-created resources:                       │ │
│ │ - Pipeline pods (pl-*)                       │ │
│ │ - Agent pods (agent-*)                       │ │
│ │ - Preview namespaces (preview-*)             │ │
│ │ - Deployment resources                       │ │
│ └──────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────┘
         ↕ port-forward
┌──────────────────────────────────────────────────┐
│ Test Runner (cargo nextest)                       │
│ - Connects to real DB, Valkey, MinIO, K8s        │
│ - Creates git repos in tempdir                    │
│ - Spawns wiremock servers for webhooks            │
│ - Makes HTTP requests to platform API             │
└──────────────────────────────────────────────────┘
```

### Test Isolation

- Each test creates its own project, user, and resources
- Git repos created in `tempfile::tempdir()` (auto-cleaned)
- K8s resources labeled with test-specific UUIDs for cleanup
- Wiremock servers listen on random ports per test

---

## Test Infrastructure

### E2E Helper Module (`tests/e2e_helpers/mod.rs`)

```rust
use std::path::PathBuf;
use tempfile::TempDir;

/// Full AppState with real K8s, MinIO, Valkey, and Postgres
pub async fn e2e_state(pool: PgPool) -> AppState {
    // Same as integration test_state but with:
    // - Real kube::Client (from KUBECONFIG)
    // - Real MinIO (from MINIO_URL)
    // - Real Valkey
    // ...
}

/// Create a bare git repo in a tempdir, return (TempDir, PathBuf)
pub fn create_bare_repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = dir.path().join("test.git");
    std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&repo_path)
        .output()
        .unwrap();
    (dir, repo_path)
}

/// Create a working copy with an initial commit, return (TempDir, PathBuf)
pub fn create_working_copy(bare_path: &Path) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let work_path = dir.path().join("work");
    std::process::Command::new("git")
        .args(["clone", bare_path.to_str().unwrap(), work_path.to_str().unwrap()])
        .output()
        .unwrap();
    // Create initial commit
    std::fs::write(work_path.join("README.md"), "# Test Project").unwrap();
    git_cmd(&work_path, &["add", "."]);
    git_cmd(&work_path, &["commit", "-m", "initial commit"]);
    git_cmd(&work_path, &["push", "origin", "main"]);
    (dir, work_path)
}

/// Run a git command in a directory
pub fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {} failed: {}",
        args.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

/// Wait for a K8s pod to reach a terminal state (max 60s)
pub async fn wait_for_pod(kube: &kube::Client, namespace: &str, name: &str, timeout_secs: u64) -> String {
    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_secs() > timeout_secs {
            panic!("pod {name} did not complete within {timeout_secs}s");
        }
        if let Ok(pod) = pods.get(name).await {
            if let Some(status) = &pod.status {
                if let Some(phase) = &status.phase {
                    if matches!(phase.as_str(), "Succeeded" | "Failed") {
                        return phase.clone();
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Cleanup K8s resources by label selector
pub async fn cleanup_k8s(kube: &kube::Client, namespace: &str, label: &str) {
    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let lp = ListParams::default().labels(label);
    if let Ok(list) = pods.list(&lp).await {
        for pod in list.items {
            if let Some(name) = pod.metadata.name {
                pods.delete(&name, &Default::default()).await.ok();
            }
        }
    }
}
```

### Justfile Addition

```just
test-e2e:
    @echo "Requires Kind cluster: just cluster-up"
    cargo nextest run --test 'e2e_*' --test-threads 2

ci-full: fmt lint deny test-unit test-integration test-e2e build
    @echo "All checks passed (including E2E tests)"
```

E2E tests run with `--test-threads 2` to limit K8s resource contention.

---

## Test File Structure

```
tests/
  e2e_helpers/
    mod.rs                    # E2E test infrastructure
  e2e_git.rs                  # Git operation tests (8 tests)
  e2e_pipeline.rs             # Pipeline execution tests (10 tests)
  e2e_webhook.rs              # Webhook dispatch tests (6 tests)
  e2e_deployer.rs             # Deployer reconciliation tests (8 tests)
  e2e_agent.rs                # Agent session lifecycle tests (8 tests)
```

---

## Test Specifications

### `tests/e2e_git.rs` — Git Operation Tests (8 tests)

| # | Test | What It Verifies | Key Assertions |
|---|------|-----------------|----------------|
| 1 | `bare_repo_init_on_project_create` | Creating a project initializes a bare git repo | Repo path exists, `git rev-parse --is-bare-repository` returns true |
| 2 | `smart_http_push` | Push commits via smart HTTP protocol | POST to `/git/{project}/git-receive-pack` succeeds, commits visible in bare repo |
| 3 | `smart_http_clone` | Clone via smart HTTP | GET `/git/{project}/info/refs?service=git-upload-pack` returns refs |
| 4 | `branch_listing` | List branches via browser API | `GET /api/projects/{id}/branches` returns main + feature branches |
| 5 | `tree_browsing` | Browse file tree via API | `GET /api/projects/{id}/tree?path=/` returns files |
| 6 | `blob_content` | Fetch file content via API | `GET /api/projects/{id}/blob?path=README.md` returns content |
| 7 | `commit_history` | Fetch commit log | `GET /api/projects/{id}/commits` returns commit messages |
| 8 | `merge_request_merge` | Create MR, merge via API | POST merge endpoint, verify source branch merged into target |

**Notes**:
- Tests 2-3 require Basic auth credentials in the request
- Test 8 requires a project with at least 2 branches and diverging commits
- All tests create temporary git repos that are cleaned up automatically

---

### `tests/e2e_pipeline.rs` — Pipeline Execution Tests (10 tests)

| # | Test | What It Verifies | Key Assertions |
|---|------|-----------------|----------------|
| 1 | `pipeline_trigger_and_execute` | POST trigger → executor picks up → pod runs → completes | Pipeline status transitions: pending → running → success |
| 2 | `pipeline_with_multiple_steps` | Pipeline with 3 steps executes sequentially | All 3 steps succeed, correct order |
| 3 | `pipeline_step_failure` | Step with `exit 1` → pipeline fails | Pipeline status = failure, step exit_code = 1 |
| 4 | `pipeline_cancel` | Cancel running pipeline | POST cancel, pipeline status = cancelled, remaining steps skipped |
| 5 | `step_logs_captured` | After pipeline completes, logs available | GET step logs returns non-empty content |
| 6 | `step_logs_in_minio` | Completed pipeline logs stored in MinIO | MinIO path `logs/pipelines/{id}/{step}.log` exists |
| 7 | `artifact_upload_and_download` | Step produces artifact, download via API | POST artifact, then GET download returns correct content |
| 8 | `pipeline_definition_parsing` | `.platformci.yml` in repo triggers pipeline with correct steps | Push with pipeline def → steps match YAML |
| 9 | `pipeline_branch_trigger_filter` | Pipeline def with `branches: [main]` only triggers for main | Push to feature branch → no pipeline created |
| 10 | `concurrent_pipeline_limit` | Max 5 concurrent pipelines | Trigger 7 pipelines, verify max 5 running simultaneously |

**Pod wait pattern**:
```rust
#[sqlx::test(migrations = "migrations")]
async fn pipeline_trigger_and_execute(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = helpers::test_router(state.clone());
    let token = helpers::admin_login(&app).await;

    // Create project with git repo
    let project_id = helpers::create_project(&app, &token, "pipe-test", "private").await;

    // Trigger pipeline
    let (status, body) = helpers::post_json(&app, &token,
        &format!("/api/projects/{project_id}/pipelines"),
        serde_json::json!({
            "git_ref": "refs/heads/main",
            "steps": [{
                "name": "test",
                "image": "alpine:3.19",
                "commands": ["echo hello", "echo world"]
            }]
        }),
    ).await;
    assert_eq!(status, StatusCode::CREATED);
    let pipeline_id = body["id"].as_str().unwrap();

    // Wait for completion (max 120s)
    let final_status = poll_pipeline_status(&app, &token, project_id, pipeline_id, 120).await;
    assert_eq!(final_status, "success");

    // Verify step completed
    let (status, body) = helpers::get_json(&app, &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["steps"][0]["status"], "success");
    assert_eq!(body["steps"][0]["exit_code"], 0);
}

async fn poll_pipeline_status(
    app: &Router, token: &str, project_id: Uuid, pipeline_id: &str, timeout_secs: u64,
) -> String {
    let start = std::time::Instant::now();
    loop {
        let (_, body) = helpers::get_json(app, token,
            &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
        ).await;
        let status = body["status"].as_str().unwrap_or("unknown").to_string();
        if matches!(status.as_str(), "success" | "failure" | "cancelled") {
            return status;
        }
        if start.elapsed().as_secs() > timeout_secs {
            panic!("pipeline did not complete within {timeout_secs}s, last status: {status}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
```

---

### `tests/e2e_webhook.rs` — Webhook Dispatch Tests (6 tests)

Uses `wiremock` crate to start a mock HTTP server per test.

| # | Test | What It Verifies | Key Assertions |
|---|------|-----------------|----------------|
| 1 | `webhook_fires_on_issue_create` | Create issue → webhook delivered | Mock server receives POST with correct payload |
| 2 | `webhook_hmac_signature` | Webhook with secret → HMAC header | `X-Platform-Signature: sha256={hex}` matches HMAC-SHA256 of body |
| 3 | `webhook_no_signature_without_secret` | Webhook without secret → no HMAC header | `X-Platform-Signature` header absent |
| 4 | `webhook_fires_on_pipeline_complete` | Pipeline completes → webhook delivered | Payload has `event: "build"`, `action: "completed"` |
| 5 | `webhook_timeout_doesnt_block` | Mock server delays 15s → webhook times out | Platform continues normally, webhook marked as failed |
| 6 | `webhook_concurrent_limit` | 60 webhooks fired simultaneously | Max 50 concurrent (semaphore), 10 dropped with warning |

**Pattern with wiremock**:

```rust
use wiremock::{MockServer, Mock, ResponseTemplate, matchers};

#[sqlx::test(migrations = "migrations")]
async fn webhook_fires_on_issue_create(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = helpers::test_router(state);
    let token = helpers::admin_login(&app).await;

    // Start mock server
    let mock_server = MockServer::start().await;

    // Register expectation
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Create project + webhook
    let project_id = helpers::create_project(&app, &token, "wh-test", "private").await;
    helpers::post_json(&app, &token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": format!("{}/webhook", mock_server.uri()),
            "events": ["issue"]
        }),
    ).await;

    // Create issue (triggers webhook)
    helpers::post_json(&app, &token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Test issue" }),
    ).await;

    // Wait briefly for async webhook delivery
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify webhook was received
    mock_server.verify().await;
}
```

**HMAC verification test**:

```rust
#[sqlx::test(migrations = "migrations")]
async fn webhook_hmac_signature(pool: PgPool) {
    // ... setup with secret ...
    let mock_server = MockServer::start().await;

    Mock::given(matchers::method("POST"))
        .and(matchers::header_exists("X-Platform-Signature"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock_server)
        .await;

    // ... trigger webhook ...

    // Retrieve the received request
    let requests = mock_server.received_requests().await.unwrap();
    let req = &requests[0];

    // Verify HMAC
    let signature = req.headers.get("X-Platform-Signature").unwrap().to_str().unwrap();
    assert!(signature.starts_with("sha256="));

    let body = &req.body;
    let expected = hmac_sha256(b"test-secret", body);
    assert_eq!(signature, format!("sha256={}", hex::encode(expected)));
}
```

---

### `tests/e2e_deployer.rs` — Deployer Reconciliation Tests (8 tests)

| # | Test | What It Verifies | Key Assertions |
|---|------|-----------------|----------------|
| 1 | `deployment_creates_k8s_resources` | Create deployment → reconciler applies manifests | K8s Deployment + Service exist in namespace |
| 2 | `deployment_health_check` | Deployment reaches healthy status | DB status transitions: pending → syncing → healthy |
| 3 | `deployment_rollback` | Set desired_status='rollback' → previous image applied | Image ref reverts to previous successful deployment |
| 4 | `deployment_stop` | Set desired_status='stopped' → replicas scaled to 0 | K8s Deployment has 0 replicas |
| 5 | `deployment_update_image` | Change image_ref → reconciler updates pod | K8s Deployment container image updated |
| 6 | `deployment_history_recorded` | Each deployment action writes history | `deployment_history` table has entries |
| 7 | `preview_deployment_lifecycle` | Pipeline on feature branch → preview created → TTL cleanup | Preview namespace created and cleaned up |
| 8 | `preview_cleanup_on_mr_merge` | Preview exists → merge MR → preview stopped | Preview status = stopped, namespace deleted |

**Pattern**:

```rust
#[sqlx::test(migrations = "migrations")]
async fn deployment_creates_k8s_resources(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &token, "deploy-test", "private").await;

    // Create deployment
    let (status, _) = helpers::post_json(&app, &token,
        &format!("/api/projects/{project_id}/deployments"),
        serde_json::json!({
            "environment": "staging",
            "image_ref": "nginx:1.25"
        }),
    ).await;
    assert_eq!(status, StatusCode::CREATED);

    // Wait for reconciler (up to 30s)
    let mut attempts = 0;
    loop {
        let (_, body) = helpers::get_json(&app, &token,
            &format!("/api/projects/{project_id}/deployments/staging"),
        ).await;
        if body["current_status"] == "healthy" { break; }
        if body["current_status"] == "failed" { panic!("deployment failed"); }
        attempts += 1;
        if attempts > 15 { panic!("deployment did not become healthy in 30s"); }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Verify K8s resources exist
    let deployments: Api<k8s_openapi::api::apps::v1::Deployment> =
        Api::namespaced(state.kube.clone(), "default");
    let dep = deployments.get("deploy-test-staging").await
        .expect("K8s Deployment should exist");
    assert!(dep.spec.unwrap().template.spec.unwrap().containers[0]
        .image.as_ref().unwrap().contains("nginx:1.25"));
}
```

---

### `tests/e2e_agent.rs` — Agent Session Lifecycle Tests (8 tests)

| # | Test | What It Verifies | Key Assertions |
|---|------|-----------------|----------------|
| 1 | `agent_session_creation` | POST creates session, pod started | Session status = running, pod exists in K8s |
| 2 | `agent_identity_created` | Session creates ephemeral agent user | DB has agent user with delegated permissions |
| 3 | `agent_pod_spec_correct` | Pod has correct env vars and mounts | Env: SESSION_ID, PROJECT_ID, AGENT_ROLE, PLATFORM_API_TOKEN |
| 4 | `agent_session_stop` | Stop running session | Pod deleted, status = stopped, identity cleaned up |
| 5 | `agent_reaper_captures_logs` | Session completes, reaper stores logs | MinIO has log file at expected path |
| 6 | `agent_session_with_custom_image` | Session with config.image override | Pod container image matches override |
| 7 | `agent_role_determines_mcp_config` | Session with role=ops | Pod args include --mcp-config with ops servers |
| 8 | `agent_identity_cleanup` | Session ends → identity fully cleaned | No tokens, sessions, or delegations remain for agent user |

**Notes**:
- Agent E2E tests require the `platform-claude-runner` image to be available in the Kind cluster
- For tests that don't need actual Claude execution, use a simple test image that prints and exits
- Session creation tests verify pod spec rather than waiting for Claude to run

---

## Resource Cleanup

### Per-Test Cleanup

Each test should clean up its K8s resources:

```rust
// At end of each test (or in a defer-like pattern):
e2e_helpers::cleanup_k8s(&state.kube, "default", &format!("platform.io/test={test_id}")).await;
```

### Global Cleanup

Add to `just cluster-down` or as a separate command:

```bash
# Clean up any leftover E2E resources
kubectl delete pods -l platform.io/component=test --all-namespaces
kubectl delete namespaces -l platform.io/component=preview --all
```

---

## Files Changed

| File | Action | Description |
|------|--------|-------------|
| `Cargo.toml` | **Modify** | Add `wiremock`, `tempfile` to dev-deps |
| `tests/e2e_helpers/mod.rs` | **New** | E2E test infrastructure |
| `tests/e2e_git.rs` | **New** | Git operation tests (8) |
| `tests/e2e_pipeline.rs` | **New** | Pipeline execution tests (10) |
| `tests/e2e_webhook.rs` | **New** | Webhook dispatch tests (6) |
| `tests/e2e_deployer.rs` | **New** | Deployer reconciliation tests (8) |
| `tests/e2e_agent.rs` | **New** | Agent session lifecycle tests (8) |
| `Justfile` | **Modify** | Add `test-e2e`, update `ci-full` |

---

## Implementation Sequence

| Step | Scope | Tests | Dependencies |
|------|-------|-------|-------------|
| **F1** | E2E infrastructure | 0 (infra) | Kind cluster running |
| **F2** | Git operation tests | 8 | F1 |
| **F3** | Pipeline tests | 10 | F1, F2 (git repos for pipeline triggers) |
| **F4** | Webhook tests | 6 | F1, wiremock |
| **F5** | Deployer tests | 8 | F1, F3 (pipeline for deployment detection) |
| **F6** | Agent tests | 8 | F1, platform-claude-runner image |

**F2-F4 can be parallelized after F1. F5 depends on F3. F6 is independent after F1.**

---

## Verification

After each step:
1. `just test-e2e` — all E2E tests pass
2. `just lint` — no clippy warnings
3. K8s resources cleaned up after tests

Final:
1. `just ci-full` — unit + integration + E2E + build all pass
2. No orphaned pods/namespaces in Kind cluster
3. Test execution time < 10 minutes total

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Kind cluster unavailable | All E2E tests fail | `#[ignore]` attribute with env check; skip gracefully |
| Slow pod startup | Tests take too long | Generous timeouts (120s); pre-pull images with `kind load` |
| Port conflicts | Mock servers fail to bind | Use random ports via wiremock's `MockServer::start()` |
| Resource leaks | Kind cluster fills up | Label-based cleanup + `cleanup_k8s()` helper |
| Flaky K8s timing | Intermittent failures | Polling with backoff instead of fixed sleeps |
| Agent image not available | Agent tests fail | Pre-load with `just docker && kind load docker-image` |
| Database connection pool exhaustion | Tests crash | Limit `--test-threads 2` for E2E tests |

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New files | 7 (6 test files + 1 helper module) |
| Modified files | 2 (Cargo.toml, Justfile) |
| New E2E tests | ~40 |
| New dev dependencies | 2 (wiremock, tempfile) |
| Estimated LOC | ~1,500-2,000 |
| Minimum test time | ~5 minutes (K8s operations) |
| Maximum test time | ~10 minutes (with all timeouts) |
