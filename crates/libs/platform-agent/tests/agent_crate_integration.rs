// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `platform-agent` crate.
//!
//! Tests exercise identity, `pubsub_bridge`, and `valkey_acl` functions against
//! real Postgres (via `#[sqlx::test]`) and real Valkey (via `VALKEY_URL`).

use fred::interfaces::ClientLike;
use fred::interfaces::EventInterface;
use fred::interfaces::PubsubInterface;
use sqlx::PgPool;
use sqlx::Row;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn valkey_pool() -> fred::clients::Pool {
    let url = std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let url = url.replace("redis://:", "redis://default:");
    let config = fred::types::config::Config::from_url(&url).expect("invalid VALKEY_URL");
    let pool =
        fred::clients::Pool::new(config, None, None, None, 1).expect("valkey pool creation failed");
    pool.init().await.expect("valkey connection failed");
    pool
}

async fn seed_user(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type)
         VALUES ($1, $2, $3, 'not-a-real-hash', 'human')",
    )
    .bind(id)
    .bind(name)
    .bind(format!("{name}@test.local"))
    .execute(pool)
    .await
    .expect("seed user");
    id
}

async fn seed_workspace(pool: &PgPool, owner_id: Uuid) -> Uuid {
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

async fn seed_project(pool: &PgPool, owner_id: Uuid, workspace_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let name = format!("proj-{}", Uuid::new_v4());
    let slug = format!("slug-{}", Uuid::new_v4());
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
    id
}

/// Upsert a permission (migrations may have already seeded some). Returns `permission_id`.
async fn seed_permission(pool: &PgPool, name: &str) -> Uuid {
    let parts: Vec<&str> = name.splitn(2, ':').collect();
    let (resource, action) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        (name, "read")
    };
    sqlx::query_scalar(
        "INSERT INTO permissions (id, name, resource, action)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (name) DO UPDATE SET resource = permissions.resource
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(name)
    .bind(resource)
    .bind(action)
    .fetch_one(pool)
    .await
    .expect("seed permission")
}

/// Upsert a role (agent roles exist from migrations). Returns `role_id`.
async fn seed_role(pool: &PgPool, name: &str, permission_ids: &[Uuid]) -> Uuid {
    let role_id: Uuid = sqlx::query_scalar(
        "INSERT INTO roles (id, name)
         VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE SET name = roles.name
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("seed role");

    for perm_id in permission_ids {
        sqlx::query(
            "INSERT INTO role_permissions (role_id, permission_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(role_id)
        .bind(perm_id)
        .execute(pool)
        .await
        .expect("seed role_permission");
    }
    role_id
}

async fn assign_role(pool: &PgPool, user_id: Uuid, role_id: Uuid, project_id: Option<Uuid>) {
    sqlx::query("INSERT INTO user_roles (user_id, role_id, project_id) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(role_id)
        .bind(project_id)
        .execute(pool)
        .await
        .expect("assign role");
}

// ===========================================================================
// identity: create_agent_identity
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn create_agent_identity_dev_role(pool: PgPool) {
    let valkey = valkey_pool().await;

    let spawner_id = seed_user(&pool, &format!("spawner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, spawner_id).await;
    let project_id = seed_project(&pool, spawner_id, ws_id).await;

    // Seed permissions
    let perm_read = seed_permission(&pool, "project:read").await;
    let perm_write = seed_permission(&pool, "project:write").await;
    let perm_secret = seed_permission(&pool, "secret:read").await;

    // Ensure agent-dev role has these permissions
    seed_role(&pool, "agent-dev", &[perm_read, perm_write, perm_secret]).await;

    // Give spawner the same permissions
    let spawner_role = seed_role(
        &pool,
        &format!("spawner-{}", Uuid::new_v4()),
        &[perm_read, perm_write, perm_secret],
    )
    .await;
    assign_role(&pool, spawner_id, spawner_role, None).await;

    let session_id = Uuid::new_v4();
    let identity = platform_agent::identity::create_agent_identity(
        &pool,
        &valkey,
        session_id,
        spawner_id,
        project_id,
        ws_id,
        platform_agent::AgentRoleName::Dev,
        Some("my-project"),
    )
    .await
    .expect("create_agent_identity should succeed");

    // Verify agent user was created
    let row = sqlx::query("SELECT name, is_active FROM users WHERE id = $1")
        .bind(identity.user_id)
        .fetch_one(&pool)
        .await
        .expect("agent user should exist");
    let name: String = row.get("name");
    let is_active: bool = row.get("is_active");
    let short_id = &session_id.to_string()[..8];
    assert_eq!(name, format!("agent-{short_id}"));
    assert!(is_active);

    // Verify role assignment is project-scoped (Dev is NOT workspace-scoped)
    let role_row = sqlx::query("SELECT project_id FROM user_roles WHERE user_id = $1")
        .bind(identity.user_id)
        .fetch_one(&pool)
        .await
        .expect("user_role should exist");
    let role_project: Option<Uuid> = role_row.get("project_id");
    assert_eq!(
        role_project,
        Some(project_id),
        "Dev role should be project-scoped"
    );

    // Verify API token was created
    let token_row = sqlx::query(
        "SELECT name, scopes, project_id, scope_workspace_id, registry_tag_pattern
         FROM api_tokens WHERE user_id = $1",
    )
    .bind(identity.user_id)
    .fetch_one(&pool)
    .await
    .expect("api_token should exist");

    let token_name: String = token_row.get("name");
    assert!(token_name.contains(&session_id.to_string()));

    let scopes: Vec<String> = token_row.get("scopes");
    assert!(scopes.contains(&"project:read".to_string()));
    assert!(scopes.contains(&"project:write".to_string()));
    assert!(scopes.contains(&"secret:read".to_string()));

    // Dev role → project + workspace boundary
    let token_project: Option<Uuid> = token_row.get("project_id");
    let token_ws: Option<Uuid> = token_row.get("scope_workspace_id");
    assert_eq!(token_project, Some(project_id));
    assert_eq!(token_ws, Some(ws_id));

    // Registry tag pattern set from project_name
    let tag_pattern: Option<String> = token_row.get("registry_tag_pattern");
    assert_eq!(
        tag_pattern,
        Some(format!("my-project/session-{short_id}-*"))
    );

    // Token format
    assert!(identity.api_token.starts_with("plat_"));

    // Cleanup
    platform_auth::resolver::invalidate_permissions(&valkey, spawner_id, None)
        .await
        .ok();
}

#[sqlx::test(migrations = "../../../migrations")]
async fn create_agent_identity_manager_workspace_scoped(pool: PgPool) {
    let valkey = valkey_pool().await;

    let spawner_id = seed_user(&pool, &format!("spawner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, spawner_id).await;
    let project_id = seed_project(&pool, spawner_id, ws_id).await;

    let perm_read = seed_permission(&pool, "project:read").await;
    let perm_write = seed_permission(&pool, "project:write").await;
    let perm_run = seed_permission(&pool, "agent:run").await;
    let perm_spawn = seed_permission(&pool, "agent:spawn").await;

    seed_role(
        &pool,
        "agent-manager",
        &[perm_read, perm_write, perm_run, perm_spawn],
    )
    .await;

    let spawner_role = seed_role(
        &pool,
        &format!("spawner-{}", Uuid::new_v4()),
        &[perm_read, perm_write, perm_run, perm_spawn],
    )
    .await;
    assign_role(&pool, spawner_id, spawner_role, None).await;

    let session_id = Uuid::new_v4();
    let identity = platform_agent::identity::create_agent_identity(
        &pool,
        &valkey,
        session_id,
        spawner_id,
        project_id,
        ws_id,
        platform_agent::AgentRoleName::Manager,
        None,
    )
    .await
    .expect("create manager identity should succeed");

    // Manager role is workspace-scoped: user_role should NOT have project_id
    let role_row = sqlx::query("SELECT project_id FROM user_roles WHERE user_id = $1")
        .bind(identity.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let role_project: Option<Uuid> = role_row.get("project_id");
    assert_eq!(
        role_project, None,
        "Manager role should NOT be project-scoped"
    );

    // Token: workspace boundary only, no project boundary
    let token_row = sqlx::query(
        "SELECT project_id, scope_workspace_id, registry_tag_pattern
         FROM api_tokens WHERE user_id = $1",
    )
    .bind(identity.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let token_project: Option<Uuid> = token_row.get("project_id");
    let token_ws: Option<Uuid> = token_row.get("scope_workspace_id");
    let tag_pattern: Option<String> = token_row.get("registry_tag_pattern");
    assert_eq!(token_project, None, "Manager token: no project boundary");
    assert_eq!(token_ws, Some(ws_id), "Manager token: workspace boundary");
    assert!(tag_pattern.is_none(), "no project_name → no tag pattern");

    // Cleanup
    platform_auth::resolver::invalidate_permissions(&valkey, spawner_id, None)
        .await
        .ok();
}

#[sqlx::test(migrations = "../../../migrations")]
async fn create_agent_identity_intersects_permissions(pool: PgPool) {
    let valkey = valkey_pool().await;

    // Separate owner so spawner doesn't get workspace-derived project:write
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let spawner_id = seed_user(&pool, &format!("spawner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let project_id = seed_project(&pool, owner_id, ws_id).await;

    let perm_read = seed_permission(&pool, "project:read").await;
    let perm_write = seed_permission(&pool, "project:write").await;

    // Agent-dev role has both read + write
    seed_role(&pool, "agent-dev", &[perm_read, perm_write]).await;

    // Spawner only has read (NOT write)
    let spawner_role = seed_role(&pool, &format!("spawner-{}", Uuid::new_v4()), &[perm_read]).await;
    assign_role(&pool, spawner_id, spawner_role, None).await;

    let session_id = Uuid::new_v4();
    let identity = platform_agent::identity::create_agent_identity(
        &pool,
        &valkey,
        session_id,
        spawner_id,
        project_id,
        ws_id,
        platform_agent::AgentRoleName::Dev,
        None,
    )
    .await
    .expect("identity should succeed");

    // Effective perms = role ∩ spawner = only project:read
    let token_row = sqlx::query("SELECT scopes FROM api_tokens WHERE user_id = $1")
        .bind(identity.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let scopes: Vec<String> = token_row.get("scopes");

    assert!(
        scopes.contains(&"project:read".to_string()),
        "intersection should include project:read"
    );
    assert!(
        !scopes.contains(&"project:write".to_string()),
        "spawner lacks project:write so intersection should exclude it"
    );

    // Cleanup
    platform_auth::resolver::invalidate_permissions(&valkey, spawner_id, None)
        .await
        .ok();
}

// ===========================================================================
// identity: cleanup_agent_identity
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn cleanup_agent_identity_removes_all(pool: PgPool) {
    let valkey = valkey_pool().await;

    let spawner_id = seed_user(&pool, &format!("spawner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, spawner_id).await;
    let project_id = seed_project(&pool, spawner_id, ws_id).await;

    let perm_read = seed_permission(&pool, "project:read").await;
    seed_role(&pool, "agent-dev", &[perm_read]).await;

    let spawner_role = seed_role(&pool, &format!("spawner-{}", Uuid::new_v4()), &[perm_read]).await;
    assign_role(&pool, spawner_id, spawner_role, None).await;

    let session_id = Uuid::new_v4();
    let identity = platform_agent::identity::create_agent_identity(
        &pool,
        &valkey,
        session_id,
        spawner_id,
        project_id,
        ws_id,
        platform_agent::AgentRoleName::Dev,
        None,
    )
    .await
    .expect("create identity");

    let agent_uid = identity.user_id;

    // Verify agent is active before cleanup
    let active: bool = sqlx::query_scalar("SELECT is_active FROM users WHERE id = $1")
        .bind(agent_uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(active, "agent should be active before cleanup");

    // Cleanup
    platform_agent::identity::cleanup_agent_identity(&pool, &valkey, agent_uid)
        .await
        .expect("cleanup should succeed");

    // Roles deleted
    let role_count: i64 = sqlx::query_scalar("SELECT count(*) FROM user_roles WHERE user_id = $1")
        .bind(agent_uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(role_count, 0, "roles should be deleted");

    // Tokens deleted
    let token_count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE user_id = $1")
        .bind(agent_uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(token_count, 0, "tokens should be deleted");

    // User deactivated
    let active_after: bool = sqlx::query_scalar("SELECT is_active FROM users WHERE id = $1")
        .bind(agent_uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!active_after, "user should be deactivated after cleanup");

    // Cleanup cache
    platform_auth::resolver::invalidate_permissions(&valkey, spawner_id, None)
        .await
        .ok();
}

// ===========================================================================
// pubsub_bridge: publish + subscribe roundtrip
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn publish_and_subscribe_events_roundtrip(_pool: PgPool) {
    let valkey = valkey_pool().await;
    let session_id = Uuid::new_v4();

    // Subscribe first (subscription established before publish)
    let mut rx = platform_agent::pubsub_bridge::subscribe_session_events(&valkey, session_id)
        .await
        .expect("subscribe should succeed");

    // Small delay for subscription propagation
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Publish a text event
    let event = platform_agent::ProgressEvent {
        kind: platform_agent::ProgressKind::Text,
        message: "hello from test".into(),
        metadata: Some(serde_json::json!({"key": "value"})),
    };
    platform_agent::pubsub_bridge::publish_event(&valkey, session_id, &event)
        .await
        .expect("publish should succeed");

    // Receive
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("should receive within timeout")
        .expect("channel should not be closed");

    assert_eq!(received.kind, platform_agent::ProgressKind::Text);
    assert_eq!(received.message, "hello from test");
    assert_eq!(received.metadata, Some(serde_json::json!({"key": "value"})));

    // Send terminal to cleanly exit subscriber task
    let terminal = platform_agent::ProgressEvent {
        kind: platform_agent::ProgressKind::Completed,
        message: "done".into(),
        metadata: None,
    };
    platform_agent::pubsub_bridge::publish_event(&valkey, session_id, &terminal)
        .await
        .expect("publish terminal");

    let term = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("should receive terminal")
        .expect("channel open");
    assert_eq!(term.kind, platform_agent::ProgressKind::Completed);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn publish_prompt_reaches_input_channel(_pool: PgPool) {
    let valkey = valkey_pool().await;
    let session_id = Uuid::new_v4();
    let channel = platform_agent::valkey_acl::input_channel(session_id);

    // Manual subscriber on the input channel
    let subscriber = valkey.next().clone_new();
    subscriber.init().await.expect("init subscriber");
    subscriber.subscribe(&channel).await.expect("subscribe");
    let mut msg_rx = subscriber.message_rx();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Publish a prompt
    platform_agent::pubsub_bridge::publish_prompt(&valkey, session_id, "test prompt")
        .await
        .expect("publish prompt should succeed");

    // Receive
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), msg_rx.recv())
        .await
        .expect("should receive within timeout")
        .expect("should not be closed");

    let payload: String = msg.value.convert().expect("convert payload");
    let parsed: serde_json::Value = serde_json::from_str(&payload).expect("parse JSON");
    assert_eq!(parsed["type"], "prompt");
    assert_eq!(parsed["content"], "test prompt");
    assert_eq!(parsed["source"], "user");

    subscriber.unsubscribe(&channel).await.ok();
}

// ===========================================================================
// valkey_acl: create + delete session ACL
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn create_and_delete_session_acl(_pool: PgPool) {
    let valkey = valkey_pool().await;
    let session_id = Uuid::new_v4();

    let creds =
        platform_agent::valkey_acl::create_session_acl(&valkey, session_id, "localhost:6379")
            .await
            .expect("create ACL should succeed");

    assert_eq!(creds.username, format!("session-{session_id}"));
    assert_eq!(creds.password.len(), 64, "password should be 64 hex chars");
    assert!(creds.url.starts_with("redis://"));
    assert!(creds.url.contains(&creds.username));

    // Delete
    platform_agent::valkey_acl::delete_session_acl(&valkey, session_id)
        .await
        .expect("delete ACL should succeed");

    // Double-delete should also succeed (idempotent)
    platform_agent::valkey_acl::delete_session_acl(&valkey, session_id)
        .await
        .expect("second delete should also succeed");
}

// ===========================================================================
// commands: resolve_command — project → workspace → global hierarchy
// ===========================================================================

async fn seed_global_command(pool: &PgPool, name: &str, template: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO platform_commands (id, name, prompt_template, description)
         VALUES ($1, $2, $3, '')
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(name)
    .bind(template)
    .fetch_one(pool)
    .await
    .expect("seed global command")
}

async fn seed_workspace_command(pool: &PgPool, ws_id: Uuid, name: &str, template: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO platform_commands (id, workspace_id, name, prompt_template, description)
         VALUES ($1, $2, $3, $4, '')
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(ws_id)
    .bind(name)
    .bind(template)
    .fetch_one(pool)
    .await
    .expect("seed workspace command")
}

async fn seed_project_command(
    pool: &PgPool,
    project_id: Uuid,
    name: &str,
    template: &str,
    persistent: bool,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO platform_commands (id, project_id, name, prompt_template, description, persistent_session)
         VALUES ($1, $2, $3, $4, '', $5)
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(project_id)
    .bind(name)
    .bind(template)
    .bind(persistent)
    .fetch_one(pool)
    .await
    .expect("seed project command")
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_command_global(pool: PgPool) {
    seed_global_command(&pool, "review", "Review the code: $ARGUMENTS").await;

    let resolved = platform_agent::commands::resolve_command(
        &pool,
        None, // no project
        None, // no workspace
        "/review fix auth",
    )
    .await
    .expect("resolve should succeed");

    assert_eq!(resolved.name, "review");
    assert_eq!(resolved.prompt, "Review the code: fix auth");
    assert!(!resolved.persistent);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_command_workspace_overrides_global(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;

    seed_global_command(&pool, "dev", "Global dev: $ARGUMENTS").await;
    seed_workspace_command(&pool, ws_id, "dev", "Workspace dev: $ARGUMENTS").await;

    let resolved =
        platform_agent::commands::resolve_command(&pool, None, Some(ws_id), "/dev fix bug")
            .await
            .expect("resolve should succeed");

    assert_eq!(resolved.name, "dev");
    assert_eq!(resolved.prompt, "Workspace dev: fix bug");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_command_project_overrides_workspace_and_global(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let project_id = seed_project(&pool, owner_id, ws_id).await;

    seed_global_command(&pool, "plan", "Global plan: $ARGUMENTS").await;
    seed_workspace_command(&pool, ws_id, "plan", "Workspace plan: $ARGUMENTS").await;
    seed_project_command(&pool, project_id, "plan", "Project plan: $ARGUMENTS", true).await;

    let resolved = platform_agent::commands::resolve_command(
        &pool,
        Some(project_id),
        Some(ws_id),
        "/plan add caching",
    )
    .await
    .expect("resolve should succeed");

    assert_eq!(resolved.name, "plan");
    assert_eq!(resolved.prompt, "Project plan: add caching");
    assert!(resolved.persistent, "project command should be persistent");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_command_not_found_returns_error(pool: PgPool) {
    let result =
        platform_agent::commands::resolve_command(&pool, None, None, "/nonexistent args").await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("not found"), "error: {err}");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_command_not_a_command_returns_error(pool: PgPool) {
    let result =
        platform_agent::commands::resolve_command(&pool, None, None, "not a command").await;

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("must start with /")
    );
}

// ===========================================================================
// commands: resolve_all_commands — merged hierarchy
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_all_commands_merges_hierarchy(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let project_id = seed_project(&pool, owner_id, ws_id).await;

    // Global: dev, review
    seed_global_command(&pool, "dev", "Global dev").await;
    seed_global_command(&pool, "review", "Global review").await;
    // Workspace: dev (overrides global), ops (new)
    seed_workspace_command(&pool, ws_id, "dev", "Workspace dev").await;
    seed_workspace_command(&pool, ws_id, "ops", "Workspace ops").await;
    // Project: dev (overrides workspace+global)
    seed_project_command(&pool, project_id, "dev", "Project dev", false).await;

    let all = platform_agent::commands::resolve_all_commands(&pool, project_id, Some(ws_id))
        .await
        .expect("resolve_all should succeed");

    // Should have: dev (project), review (global), ops (workspace)
    assert_eq!(all.len(), 3, "should have 3 merged commands");

    let dev = all.iter().find(|c| c.name == "dev").unwrap();
    assert_eq!(dev.scope, "project", "dev should come from project tier");
    assert_eq!(dev.prompt_template, "Project dev");

    let review = all.iter().find(|c| c.name == "review").unwrap();
    assert_eq!(
        review.scope, "global",
        "review should come from global tier"
    );

    let ops = all.iter().find(|c| c.name == "ops").unwrap();
    assert_eq!(
        ops.scope, "workspace",
        "ops should come from workspace tier"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_all_commands_empty_project_returns_empty(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let project_id = seed_project(&pool, owner_id, ws_id).await;

    // No commands at any tier
    let all = platform_agent::commands::resolve_all_commands(&pool, project_id, Some(ws_id))
        .await
        .expect("resolve_all should succeed");

    assert!(all.is_empty(), "no commands should be returned");
}

// ===========================================================================
// service: fetch_session — DB round-trip
// ===========================================================================

async fn seed_agent_session(pool: &PgPool, user_id: Uuid, project_id: Uuid) -> Uuid {
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, execution_mode, uses_pubsub)
         VALUES ($1, $2, $3, 'test prompt', 'running', 'claude-code', 'pod', true)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed agent session");
    session_id
}

#[sqlx::test(migrations = "../../../migrations")]
async fn fetch_session_returns_session(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let project_id = seed_project(&pool, owner_id, ws_id).await;
    let session_id = seed_agent_session(&pool, owner_id, project_id).await;

    let session = platform_agent::service::fetch_session(&pool, session_id)
        .await
        .expect("fetch_session should succeed");

    assert_eq!(session.id, session_id);
    assert_eq!(session.project_id, Some(project_id));
    assert_eq!(session.user_id, owner_id);
    assert_eq!(session.prompt, "test prompt");
    assert_eq!(session.status, "running");
    assert_eq!(session.provider, "claude-code");
    assert_eq!(session.execution_mode, "pod");
    assert!(session.uses_pubsub);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn fetch_session_not_found_returns_error(pool: PgPool) {
    let result = platform_agent::service::fetch_session(&pool, Uuid::new_v4()).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        platform_agent::AgentError::SessionNotFound => {}
        other => panic!("expected SessionNotFound, got: {other}"),
    }
}
