// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `platform-secrets` crate.
//!
//! Tests all CRUD operations against a real Postgres via `#[sqlx::test]`.

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use platform_secrets::{cli_creds, engine, llm_providers, user_keys};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn dev_key() -> [u8; 32] {
    engine::dev_master_key()
}

/// Seed a minimal user. Returns `user_id`.
async fn seed_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let name = format!("u-{id}");
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type)
         VALUES ($1, $2, $3, 'not-a-real-hash', 'human')",
    )
    .bind(id)
    .bind(&name)
    .bind(format!("{name}@test.local"))
    .execute(pool)
    .await
    .expect("seed user");
    id
}

/// Seed a workspace + project. Returns (`workspace_id`, `project_id`).
async fn seed_project(pool: &PgPool, owner_id: Uuid) -> (Uuid, Uuid) {
    let ws_id = Uuid::new_v4();
    let ws_name = format!("ws-{ws_id}");
    sqlx::query("INSERT INTO workspaces (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(ws_id)
        .bind(&ws_name)
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
    .expect("seed workspace member");

    let proj_id = Uuid::new_v4();
    let proj_name = format!("proj-{proj_id}");
    let slug = format!("slug-{proj_id}");
    sqlx::query(
        "INSERT INTO projects (id, owner_id, workspace_id, name, namespace_slug)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(proj_id)
    .bind(owner_id)
    .bind(ws_id)
    .bind(&proj_name)
    .bind(&slug)
    .execute(pool)
    .await
    .expect("seed project");

    (ws_id, proj_id)
}

// ===========================================================================
// engine.rs — CRUD
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn create_and_resolve_secret(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    let meta = engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "DB_URL",
            value: b"postgres://localhost/db",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .expect("create_secret");

    assert_eq!(meta.name, "DB_URL");
    assert_eq!(meta.project_id, Some(project_id));
    assert_eq!(meta.version, 1);

    let resolved = engine::resolve_secret(&pool, &key, project_id, "DB_URL", "pipeline")
        .await
        .expect("resolve_secret");
    assert_eq!(resolved, "postgres://localhost/db");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn create_and_resolve_global_secret(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    engine::create_global_secret(&pool, &key, "GLOBAL_KEY", b"global-value", "all", owner)
        .await
        .expect("create_global_secret");

    let resolved = engine::resolve_global_secret(&pool, &key, "GLOBAL_KEY", "pipeline")
        .await
        .expect("resolve_global_secret");
    assert_eq!(resolved, "global-value");

    // Global secret should also be found via resolve_secret (fallback)
    let resolved2 = engine::resolve_secret(&pool, &key, project_id, "GLOBAL_KEY", "all")
        .await
        .expect("resolve_secret fallback to global");
    assert_eq!(resolved2, "global-value");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn delete_secret_project_scoped(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "DEL_ME",
            value: b"val",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    let deleted = engine::delete_secret(&pool, Some(project_id), "DEL_ME")
        .await
        .unwrap();
    assert!(deleted);

    // Second delete returns false
    let deleted2 = engine::delete_secret(&pool, Some(project_id), "DEL_ME")
        .await
        .unwrap();
    assert!(!deleted2);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn delete_global_secret(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;

    engine::create_global_secret(&pool, &key, "GLOBAL_DEL", b"val", "all", owner)
        .await
        .unwrap();

    let deleted = engine::delete_secret(&pool, None, "GLOBAL_DEL")
        .await
        .unwrap();
    assert!(deleted);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn list_secrets_project_scoped(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "SECRET_A",
            value: b"a",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "SECRET_B",
            value: b"b",
            scope: "pipeline",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    let list = engine::list_secrets(&pool, Some(project_id), None)
        .await
        .unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].name, "SECRET_A");
    assert_eq!(list[1].name, "SECRET_B");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn list_secrets_global(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;

    engine::create_global_secret(&pool, &key, "GLIST_A", b"a", "all", owner)
        .await
        .unwrap();

    let list = engine::list_secrets(&pool, None, None).await.unwrap();
    assert!(list.iter().any(|s| s.name == "GLIST_A"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn list_workspace_secrets(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (ws_id, _) = seed_project(&pool, owner).await;

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: None,
            workspace_id: Some(ws_id),
            environment: None,
            name: "WS_SECRET",
            value: b"ws-val",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    let list = engine::list_workspace_secrets(&pool, ws_id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "WS_SECRET");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secret_hierarchical_all_levels(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (ws_id, project_id) = seed_project(&pool, owner).await;

    // Level 4: global
    engine::create_global_secret(&pool, &key, "HIER_KEY", b"global", "all", owner)
        .await
        .unwrap();
    let v = engine::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        Some(ws_id),
        Some("staging"),
        "HIER_KEY",
        "all",
    )
    .await
    .unwrap();
    assert_eq!(v, "global");

    // Level 3: workspace (overrides global)
    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: None,
            workspace_id: Some(ws_id),
            environment: None,
            name: "HIER_KEY",
            value: b"workspace",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();
    let v = engine::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        Some(ws_id),
        Some("staging"),
        "HIER_KEY",
        "all",
    )
    .await
    .unwrap();
    assert_eq!(v, "workspace");

    // Level 2: project (overrides workspace)
    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "HIER_KEY",
            value: b"project",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();
    let v = engine::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        Some(ws_id),
        Some("staging"),
        "HIER_KEY",
        "all",
    )
    .await
    .unwrap();
    assert_eq!(v, "project");

    // Level 1: project+env (overrides project)
    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: Some("staging"),
            name: "HIER_KEY",
            value: b"project-staging",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();
    let v = engine::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        Some(ws_id),
        Some("staging"),
        "HIER_KEY",
        "all",
    )
    .await
    .unwrap();
    assert_eq!(v, "project-staging");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secret_scope_mismatch(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "SCOPED",
            value: b"val",
            scope: "pipeline",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    // Request with "agent" scope should fail
    let err = engine::resolve_secret(&pool, &key, project_id, "SCOPED", "agent")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("scope"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secret_not_found(pool: PgPool) {
    let key = dev_key();
    let err = engine::resolve_secret(&pool, &key, Uuid::new_v4(), "NONEXISTENT", "all")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secrets_for_env_template(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "HOST",
            value: b"db.example.com",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "PORT",
            value: b"5432",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    let result = engine::resolve_secrets_for_env(
        &pool,
        &key,
        project_id,
        "all",
        "url=${{ secrets.HOST }}:${{ secrets.PORT }}",
    )
    .await
    .unwrap();
    assert_eq!(result, "url=db.example.com:5432");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secrets_for_env_missing_secret_kept(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    let result = engine::resolve_secrets_for_env(
        &pool,
        &key,
        project_id,
        "all",
        "val=${{ secrets.MISSING }}",
    )
    .await
    .unwrap();
    // Missing secrets are left as-is (not replaced)
    assert_eq!(result, "val=${{ secrets.MISSING }}");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn query_scoped_secrets_roundtrip(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "QS_A",
            value: b"val-a",
            scope: "pipeline",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "QS_B",
            value: b"val-b",
            scope: "agent",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    // Only fetch pipeline-scoped
    let secrets = engine::query_scoped_secrets(&pool, &key, project_id, &["pipeline"], None)
        .await
        .unwrap();
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0].0, "QS_A");
    assert_eq!(secrets[0].1, "val-a");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn create_secret_upsert_increments_version(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    let m1 = engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "UPSERT",
            value: b"v1",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();
    assert_eq!(m1.version, 1);

    let m2 = engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "UPSERT",
            value: b"v2",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();
    assert_eq!(m2.version, 2);
    assert_eq!(m2.id, m1.id); // same row

    let resolved = engine::resolve_secret(&pool, &key, project_id, "UPSERT", "all")
        .await
        .unwrap();
    assert_eq!(resolved, "v2");
}

// ===========================================================================
// llm_providers.rs
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_create_get_roundtrip(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let env_vars = std::collections::HashMap::from([
        (
            "ANTHROPIC_BASE_URL".to_string(),
            "https://example.com".to_string(),
        ),
        ("ANTHROPIC_API_KEY".to_string(), "sk-test-123".to_string()),
    ]);

    let config_id = llm_providers::create_config(
        &pool,
        &key,
        user_id,
        "custom_endpoint",
        "My Custom",
        &env_vars,
        Some("claude-3"),
    )
    .await
    .expect("create_config");

    let config = llm_providers::get_config(&pool, &key, config_id, user_id)
        .await
        .expect("get_config")
        .expect("should be Some");

    assert_eq!(config.provider_type, "custom_endpoint");
    assert_eq!(config.label, "My Custom");
    assert_eq!(config.model.as_deref(), Some("claude-3"));
    assert_eq!(config.env_vars["ANTHROPIC_BASE_URL"], "https://example.com");
    assert_eq!(config.env_vars["ANTHROPIC_API_KEY"], "sk-test-123");
    assert_eq!(config.validation_status, "untested");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_update_config(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let env_vars = std::collections::HashMap::from([
        (
            "ANTHROPIC_BASE_URL".to_string(),
            "https://old.com".to_string(),
        ),
        ("ANTHROPIC_API_KEY".to_string(), "sk-old".to_string()),
    ]);

    let config_id = llm_providers::create_config(
        &pool,
        &key,
        user_id,
        "custom_endpoint",
        "Old Label",
        &env_vars,
        None,
    )
    .await
    .unwrap();

    let new_vars = std::collections::HashMap::from([
        (
            "ANTHROPIC_BASE_URL".to_string(),
            "https://new.com".to_string(),
        ),
        ("ANTHROPIC_API_KEY".to_string(), "sk-new".to_string()),
    ]);

    let updated = llm_providers::update_config(
        &pool,
        &key,
        config_id,
        user_id,
        &new_vars,
        Some("claude-4"),
        "New Label",
    )
    .await
    .unwrap();
    assert!(updated);

    let config = llm_providers::get_config(&pool, &key, config_id, user_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(config.label, "New Label");
    assert_eq!(config.env_vars["ANTHROPIC_BASE_URL"], "https://new.com");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_update_config_not_found(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;
    let vars = std::collections::HashMap::new();

    let updated =
        llm_providers::update_config(&pool, &key, Uuid::new_v4(), user_id, &vars, None, "label")
            .await
            .unwrap();
    assert!(!updated);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_list_configs(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let vars1 = std::collections::HashMap::from([
        ("AWS_ACCESS_KEY_ID".to_string(), "key1".to_string()),
        ("AWS_SECRET_ACCESS_KEY".to_string(), "secret1".to_string()),
    ]);
    llm_providers::create_config(&pool, &key, user_id, "bedrock", "Config 1", &vars1, None)
        .await
        .unwrap();

    let vars2 = std::collections::HashMap::from([(
        "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
        "proj1".to_string(),
    )]);
    llm_providers::create_config(&pool, &key, user_id, "vertex", "Config 2", &vars2, None)
        .await
        .unwrap();

    let list = llm_providers::list_configs(&pool, user_id).await.unwrap();
    assert_eq!(list.len(), 2);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_delete_config(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let vars = std::collections::HashMap::from([(
        "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
        "proj".to_string(),
    )]);
    let config_id =
        llm_providers::create_config(&pool, &key, user_id, "vertex", "To Delete", &vars, None)
            .await
            .unwrap();

    let deleted = llm_providers::delete_config(&pool, config_id, user_id)
        .await
        .unwrap();
    assert!(deleted);

    // Deleted
    let config = llm_providers::get_config(&pool, &key, config_id, user_id)
        .await
        .unwrap();
    assert!(config.is_none());

    // Second delete returns false
    let deleted2 = llm_providers::delete_config(&pool, config_id, user_id)
        .await
        .unwrap();
    assert!(!deleted2);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_delete_active_reverts_to_auto(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let vars = std::collections::HashMap::from([(
        "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
        "proj".to_string(),
    )]);
    let config_id =
        llm_providers::create_config(&pool, &key, user_id, "vertex", "Active", &vars, None)
            .await
            .unwrap();

    // Set as active
    let active_value = format!("custom:{config_id}");
    llm_providers::set_active_provider(&pool, user_id, &active_value)
        .await
        .unwrap();

    // Delete should revert to 'auto'
    llm_providers::delete_config(&pool, config_id, user_id)
        .await
        .unwrap();

    let active = llm_providers::get_active_provider(&pool, user_id)
        .await
        .unwrap();
    assert_eq!(active, "auto");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_get_set_active_provider(pool: PgPool) {
    let user_id = seed_user(&pool).await;

    let active = llm_providers::get_active_provider(&pool, user_id)
        .await
        .unwrap();
    assert_eq!(active, "auto"); // default

    llm_providers::set_active_provider(&pool, user_id, "bedrock")
        .await
        .unwrap();
    let active = llm_providers::get_active_provider(&pool, user_id)
        .await
        .unwrap();
    assert_eq!(active, "bedrock");
}

// ===========================================================================
// cli_creds.rs
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn cli_creds_store_and_decrypt_roundtrip(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let info = cli_creds::store_credentials(
        &pool,
        &key,
        user_id,
        "oauth",
        r#"{"access_token":"at-123"}"#,
        Some(Utc::now() + chrono::Duration::hours(1)),
    )
    .await
    .expect("store_credentials");

    assert_eq!(info.user_id, user_id);
    assert_eq!(info.auth_type, "oauth");
    assert!(info.token_expires_at.is_some());

    let decrypted = cli_creds::get_decrypted_credential(&pool, &key, user_id)
        .await
        .expect("get_decrypted")
        .expect("should be Some");
    assert_eq!(decrypted.auth_type, "oauth");
    assert_eq!(decrypted.value, r#"{"access_token":"at-123"}"#);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn cli_creds_get_credential_info(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    // Not found
    let info = cli_creds::get_credential_info(&pool, user_id)
        .await
        .unwrap();
    assert!(info.is_none());

    // Store and retrieve
    cli_creds::store_credentials(&pool, &key, user_id, "setup_token", "tok-123", None)
        .await
        .unwrap();

    let info = cli_creds::get_credential_info(&pool, user_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.auth_type, "setup_token");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn cli_creds_delete_credentials(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    cli_creds::store_credentials(&pool, &key, user_id, "oauth", "tok", None)
        .await
        .unwrap();

    let deleted = cli_creds::delete_credentials(&pool, user_id).await.unwrap();
    assert!(deleted);

    let deleted2 = cli_creds::delete_credentials(&pool, user_id).await.unwrap();
    assert!(!deleted2);

    let info = cli_creds::get_credential_info(&pool, user_id)
        .await
        .unwrap();
    assert!(info.is_none());
}

#[sqlx::test(migrations = "../../../migrations")]
async fn cli_creds_resolve_cli_auth(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    // No credentials → None
    let auth = cli_creds::resolve_cli_auth(&pool, &key, user_id)
        .await
        .unwrap();
    assert!(auth.is_none());

    // Store and resolve
    cli_creds::store_credentials(&pool, &key, user_id, "oauth", "my-token", None)
        .await
        .unwrap();

    let auth = cli_creds::resolve_cli_auth(&pool, &key, user_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(auth, "my-token");
}

// ===========================================================================
// user_keys.rs
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn user_keys_set_get_roundtrip(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    user_keys::set_user_key(&pool, &key, user_id, "anthropic", "sk-ant-test-abcd1234")
        .await
        .expect("set_user_key");

    let retrieved = user_keys::get_user_key(&pool, &key, user_id, "anthropic")
        .await
        .expect("get_user_key")
        .expect("should be Some");
    assert_eq!(retrieved, "sk-ant-test-abcd1234");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn user_keys_get_not_found(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let result = user_keys::get_user_key(&pool, &key, user_id, "nonexistent")
        .await
        .unwrap();
    assert!(result.is_none());
}

#[sqlx::test(migrations = "../../../migrations")]
async fn user_keys_list(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    user_keys::set_user_key(&pool, &key, user_id, "anthropic", "sk-ant-1234")
        .await
        .unwrap();
    user_keys::set_user_key(&pool, &key, user_id, "openai", "sk-oai-5678")
        .await
        .unwrap();

    let list = user_keys::list_user_keys(&pool, user_id).await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().any(|k| k.provider == "anthropic"));
    assert!(list.iter().any(|k| k.provider == "openai"));
    // Verify suffix is masked
    let anthro = list.iter().find(|k| k.provider == "anthropic").unwrap();
    assert_eq!(anthro.key_suffix, "...1234");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn user_keys_delete(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    user_keys::set_user_key(&pool, &key, user_id, "anthropic", "sk-test")
        .await
        .unwrap();

    let deleted = user_keys::delete_user_key(&pool, user_id, "anthropic")
        .await
        .unwrap();
    assert!(deleted);

    let deleted2 = user_keys::delete_user_key(&pool, user_id, "anthropic")
        .await
        .unwrap();
    assert!(!deleted2);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn user_keys_upsert_replaces(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    user_keys::set_user_key(&pool, &key, user_id, "anthropic", "old-key-1234")
        .await
        .unwrap();
    user_keys::set_user_key(&pool, &key, user_id, "anthropic", "new-key-5678")
        .await
        .unwrap();

    let val = user_keys::get_user_key(&pool, &key, user_id, "anthropic")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(val, "new-key-5678");

    // Should still be 1 key
    let list = user_keys::list_user_keys(&pool, user_id).await.unwrap();
    assert_eq!(list.len(), 1);
}

// ===========================================================================
// PgSecretsResolver trait impl
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn pg_secrets_resolver_trait(pool: PgPool) {
    use platform_types::SecretsResolver;

    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (ws_id, project_id) = seed_project(&pool, owner).await;

    engine::create_secret(
        &pool,
        &key,
        engine::CreateSecretParams {
            project_id: Some(project_id),
            workspace_id: None,
            environment: None,
            name: "TRAIT_SECRET",
            value: b"trait-val",
            scope: "all",
            created_by: owner,
        },
    )
    .await
    .unwrap();

    let resolver = engine::PgSecretsResolver {
        pool: &pool,
        master_key: &key,
    };

    // resolve_secret
    let val = resolver
        .resolve_secret(project_id, "TRAIT_SECRET", "all")
        .await
        .unwrap();
    assert_eq!(val, "trait-val");

    // resolve_secret_hierarchical
    let val = resolver
        .resolve_secret_hierarchical(project_id, Some(ws_id), None, "TRAIT_SECRET", "all")
        .await
        .unwrap();
    assert_eq!(val, "trait-val");

    // resolve_secrets_for_env
    let val = resolver
        .resolve_secrets_for_env(project_id, "all", "v=${{ secrets.TRAIT_SECRET }}")
        .await
        .unwrap();
    assert_eq!(val, "v=trait-val");
}

// ===========================================================================
// Coverage gap tests — error paths & edge cases
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_global_secret_not_found(pool: PgPool) {
    let key = dev_key();
    let err = engine::resolve_global_secret(&pool, &key, "NO_SUCH_GLOBAL", "all")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secret_hierarchical_not_found(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (ws_id, project_id) = seed_project(&pool, owner).await;

    let err = engine::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        Some(ws_id),
        Some("staging"),
        "NONEXISTENT_HIER",
        "all",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secrets_for_env_unclosed_pattern(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    // Unclosed ${{ secrets.NAME (no closing " }}") — should leave template unchanged
    let result =
        engine::resolve_secrets_for_env(&pool, &key, project_id, "all", "val=${{ secrets.UNCLOSED")
            .await
            .unwrap();
    assert_eq!(result, "val=${{ secrets.UNCLOSED");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_create_invalid_provider_type(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;
    let vars = std::collections::HashMap::from([("KEY".to_string(), "val".to_string())]);

    let err = llm_providers::create_config(&pool, &key, user_id, "openai", "Bad", &vars, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid provider_type"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_create_invalid_env_vars(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;
    // bedrock requires AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY
    let vars =
        std::collections::HashMap::from([("AWS_ACCESS_KEY_ID".to_string(), "k".to_string())]);

    let err = llm_providers::create_config(&pool, &key, user_id, "bedrock", "Bad", &vars, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("AWS_SECRET_ACCESS_KEY"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_update_invalid_env_vars(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let vars = std::collections::HashMap::from([
        ("AWS_ACCESS_KEY_ID".to_string(), "key".to_string()),
        ("AWS_SECRET_ACCESS_KEY".to_string(), "secret".to_string()),
    ]);
    let config_id =
        llm_providers::create_config(&pool, &key, user_id, "bedrock", "Valid", &vars, None)
            .await
            .unwrap();

    // Update with missing required key
    let bad_vars =
        std::collections::HashMap::from([("AWS_ACCESS_KEY_ID".to_string(), "k".to_string())]);
    let err =
        llm_providers::update_config(&pool, &key, config_id, user_id, &bad_vars, None, "Updated")
            .await
            .unwrap_err();
    assert!(err.to_string().contains("AWS_SECRET_ACCESS_KEY"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_update_validation_status(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    let vars = std::collections::HashMap::from([(
        "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
        "proj".to_string(),
    )]);
    let config_id =
        llm_providers::create_config(&pool, &key, user_id, "vertex", "Test", &vars, None)
            .await
            .unwrap();

    llm_providers::update_validation_status(&pool, config_id, user_id, "valid")
        .await
        .expect("update_validation_status");

    let list = llm_providers::list_configs(&pool, user_id).await.unwrap();
    let config = list.iter().find(|c| c.id == config_id).unwrap();
    assert_eq!(config.validation_status, "valid");
    assert!(config.last_validated_at.is_some());
}

// ---------------------------------------------------------------------------
// Non-UTF-8 error paths (requires raw DB writes to bypass encrypt)
// ---------------------------------------------------------------------------

/// Encrypt raw bytes (including invalid UTF-8) and insert directly into `secrets`,
/// bypassing the text-based `create_secret` which always stores valid UTF-8 input.
async fn insert_binary_secret(
    pool: &PgPool,
    key: &[u8; 32],
    project_id: Option<Uuid>,
    name: &str,
    raw_bytes: &[u8],
    scope: &str,
    created_by: Uuid,
) {
    let encrypted = engine::encrypt(raw_bytes, key).unwrap();
    sqlx::query(
        "INSERT INTO secrets (project_id, workspace_id, environment, name, encrypted_value, scope, created_by)
         VALUES ($1, NULL, NULL, $2, $3, $4, $5)",
    )
    .bind(project_id)
    .bind(name)
    .bind(&encrypted)
    .bind(scope)
    .bind(created_by)
    .execute(pool)
    .await
    .expect("insert binary secret");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secret_non_utf8_error(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    // Store invalid UTF-8 bytes directly
    insert_binary_secret(
        &pool,
        &key,
        Some(project_id),
        "BIN_SECRET",
        &[0xFF, 0xFE],
        "all",
        owner,
    )
    .await;

    let err = engine::resolve_secret(&pool, &key, project_id, "BIN_SECRET", "all")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not valid UTF-8"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_global_secret_non_utf8_error(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;

    insert_binary_secret(&pool, &key, None, "BIN_GLOBAL", &[0xFF, 0xFE], "all", owner).await;

    let err = engine::resolve_global_secret(&pool, &key, "BIN_GLOBAL", "all")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not valid UTF-8"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_secret_hierarchical_non_utf8_error(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (ws_id, project_id) = seed_project(&pool, owner).await;

    insert_binary_secret(
        &pool,
        &key,
        Some(project_id),
        "BIN_HIER",
        &[0xFF, 0xFE],
        "all",
        owner,
    )
    .await;

    let err = engine::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        Some(ws_id),
        None,
        "BIN_HIER",
        "all",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not valid UTF-8"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn query_scoped_secrets_non_utf8_error(pool: PgPool) {
    let key = dev_key();
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    insert_binary_secret(
        &pool,
        &key,
        Some(project_id),
        "BAD_KEY",
        &[0xFF, 0xFE],
        "all",
        owner,
    )
    .await;

    // UTF-8 error propagates via `?` in the Ok(plaintext) branch
    let err = engine::query_scoped_secrets(&pool, &key, project_id, &["all"], None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not valid UTF-8"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn cli_creds_non_utf8_credential_error(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    // Store non-UTF-8 bytes directly in cli_credentials
    let encrypted = engine::encrypt(&[0xFF, 0xFE], &key).unwrap();
    sqlx::query(
        "INSERT INTO cli_credentials (user_id, auth_type, encrypted_data)
         VALUES ($1, 'oauth', $2)",
    )
    .bind(user_id)
    .bind(&encrypted)
    .execute(&pool)
    .await
    .expect("insert binary cli credential");

    let result = cli_creds::get_decrypted_credential(&pool, &key, user_id).await;
    let err = result.err().expect("should fail with non-UTF-8 error");
    assert!(err.to_string().contains("not valid UTF-8"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn user_keys_non_utf8_key_error(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    // Store non-UTF-8 bytes directly in user_provider_keys
    let encrypted = engine::encrypt(&[0xFF, 0xFE], &key).unwrap();
    sqlx::query(
        "INSERT INTO user_provider_keys (user_id, provider, encrypted_key, key_suffix)
         VALUES ($1, 'anthropic', $2, '...xx')",
    )
    .bind(user_id)
    .bind(&encrypted)
    .execute(&pool)
    .await
    .expect("insert binary user key");

    let err = user_keys::get_user_key(&pool, &key, user_id, "anthropic")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not valid UTF-8"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn llm_provider_get_config_corrupted_blob(pool: PgPool) {
    let key = dev_key();
    let user_id = seed_user(&pool).await;

    // Insert a config with valid encryption but invalid JSON inside
    let encrypted = engine::encrypt(b"not-json", &key).unwrap();
    let config_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO llm_provider_configs (id, user_id, provider_type, label, encrypted_config)
         VALUES ($1, $2, 'bedrock', 'Corrupt', $3)",
    )
    .bind(config_id)
    .bind(user_id)
    .bind(&encrypted)
    .execute(&pool)
    .await
    .expect("insert corrupted config");

    let result = llm_providers::get_config(&pool, &key, config_id, user_id).await;
    let err = result.err().expect("should fail with parse error");
    assert!(err.to_string().contains("failed to parse encrypted config"));
}
