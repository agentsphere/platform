// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Idempotent database seeder for the platform.
//!
//! Seeds permissions, roles, role-permission mappings, and the OTEL service
//! account.  All inserts use `ON CONFLICT DO NOTHING` so re-running is safe.
//!
//! The OTEL service account gets a **random** API token on first run.  The raw
//! token is stored in a K8s Secret (`otel-system-token` by default) so that
//! sidecars can mount it.  Only the SHA-256 hash is persisted in Postgres.
//! Re-running rotates the token (deletes the old DB row, creates a new one,
//! updates the K8s Secret).

use std::collections::BTreeMap;

use anyhow::Context;
use clap::Parser;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, Patch, PatchParams};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "platform-seed", about = "Seed the platform database")]
struct Cli {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// K8s namespace for secrets.
    /// Defaults to the pod's own namespace (in-cluster) or "platform".
    #[arg(long, env = "PLATFORM_NAMESPACE")]
    namespace: Option<String>,

    /// Name of the K8s Secret that holds the OTEL API token.
    #[arg(long, default_value = "otel-system-token")]
    otel_secret_name: String,

    /// Name of the K8s Secret that holds the admin password.
    #[arg(long, default_value = "admin-credentials")]
    admin_secret_name: String,

    /// Dev mode: use "admin" as the admin password instead of random.
    #[arg(long, env = "PLATFORM_DEV")]
    dev: bool,
}

// ---------------------------------------------------------------------------
// Permission definitions
// ---------------------------------------------------------------------------

struct PermDef {
    name: &'static str,
    resource: &'static str,
    action: &'static str,
    description: &'static str,
}

const PERMISSIONS: &[PermDef] = &[
    PermDef {
        name: "project:read",
        resource: "project",
        action: "read",
        description: "Read project data, issues, MRs",
    },
    PermDef {
        name: "project:write",
        resource: "project",
        action: "write",
        description: "Create/update projects, issues, MRs",
    },
    PermDef {
        name: "project:delete",
        resource: "project",
        action: "delete",
        description: "Delete projects",
    },
    PermDef {
        name: "agent:run",
        resource: "agent",
        action: "run",
        description: "Start agent sessions",
    },
    PermDef {
        name: "agent:spawn",
        resource: "agent",
        action: "spawn",
        description: "Spawn child agent sessions",
    },
    PermDef {
        name: "deploy:read",
        resource: "deploy",
        action: "read",
        description: "View deployments",
    },
    PermDef {
        name: "deploy:promote",
        resource: "deploy",
        action: "promote",
        description: "Promote deployments between environments",
    },
    PermDef {
        name: "observe:read",
        resource: "observe",
        action: "read",
        description: "Read logs, metrics, traces",
    },
    PermDef {
        name: "observe:write",
        resource: "observe",
        action: "write",
        description: "Write observability data",
    },
    PermDef {
        name: "alert:manage",
        resource: "alert",
        action: "manage",
        description: "Create and manage alert rules",
    },
    PermDef {
        name: "secret:read",
        resource: "secret",
        action: "read",
        description: "Read secret metadata (not values)",
    },
    PermDef {
        name: "secret:write",
        resource: "secret",
        action: "write",
        description: "Create and update secrets",
    },
    PermDef {
        name: "admin:users",
        resource: "admin",
        action: "users",
        description: "Manage users and roles",
    },
    PermDef {
        name: "admin:roles",
        resource: "admin",
        action: "roles",
        description: "Manage role definitions and assignments",
    },
    PermDef {
        name: "admin:config",
        resource: "admin",
        action: "config",
        description: "Manage platform configuration",
    },
    PermDef {
        name: "admin:delegate",
        resource: "admin",
        action: "delegate",
        description: "Delegate permissions to other users/agents",
    },
    PermDef {
        name: "workspace:read",
        resource: "workspace",
        action: "read",
        description: "Read workspace data",
    },
    PermDef {
        name: "workspace:write",
        resource: "workspace",
        action: "write",
        description: "Create/update workspaces",
    },
    PermDef {
        name: "workspace:admin",
        resource: "workspace",
        action: "admin",
        description: "Manage workspace members and settings",
    },
    PermDef {
        name: "registry:pull",
        resource: "registry",
        action: "pull",
        description: "Pull images from project registry",
    },
    PermDef {
        name: "registry:push",
        resource: "registry",
        action: "push",
        description: "Push images to project registry",
    },
    PermDef {
        name: "flag:manage",
        resource: "flag",
        action: "manage",
        description: "Manage feature flags",
    },
];

// ---------------------------------------------------------------------------
// Role definitions
// ---------------------------------------------------------------------------

struct RoleDef {
    name: &'static str,
    description: &'static str,
    permissions: &'static [&'static str],
    is_system: bool,
}

const ROLES: &[RoleDef] = &[
    RoleDef {
        name: "admin",
        description: "Platform administrator with full access",
        permissions: &[], // admin gets all permissions via wildcard logic
        is_system: true,
    },
    RoleDef {
        name: "developer",
        description: "Human developer with project and agent access",
        permissions: &[
            "project:read",
            "project:write",
            "agent:run",
            "agent:spawn",
            "deploy:read",
            "observe:read",
            "secret:read",
            "workspace:read",
            "workspace:write",
            "registry:pull",
            "registry:push",
        ],
        is_system: true,
    },
    RoleDef {
        name: "ops",
        description: "Operations staff with deploy and observe access",
        permissions: &[
            "deploy:read",
            "deploy:promote",
            "observe:read",
            "observe:write",
            "alert:manage",
            "secret:read",
            "registry:pull",
        ],
        is_system: true,
    },
    RoleDef {
        name: "agent",
        description: "AI agent identity — legacy role (see agent-* roles)",
        permissions: &[],
        is_system: true,
    },
    RoleDef {
        name: "viewer",
        description: "Read-only access",
        permissions: &[
            "project:read",
            "observe:read",
            "deploy:read",
            "registry:pull",
        ],
        is_system: true,
    },
    RoleDef {
        name: "agent-dev",
        description: "Agent: developer — code within a project",
        permissions: &[
            "project:read",
            "project:write",
            "secret:read",
            "registry:pull",
            "registry:push",
        ],
        is_system: false,
    },
    RoleDef {
        name: "agent-ops",
        description: "Agent: operations — deploy and observe a project",
        permissions: &[
            "project:read",
            "deploy:read",
            "deploy:promote",
            "observe:read",
            "observe:write",
            "alert:manage",
            "secret:read",
            "registry:pull",
        ],
        is_system: false,
    },
    RoleDef {
        name: "agent-test",
        description: "Agent: tester — read-only project + observability",
        permissions: &["project:read", "observe:read", "registry:pull"],
        is_system: false,
    },
    RoleDef {
        name: "agent-review",
        description: "Agent: reviewer — read-only project access",
        permissions: &["project:read", "observe:read"],
        is_system: false,
    },
    RoleDef {
        name: "agent-manager",
        description: "Agent: manager — create projects, spawn agents",
        permissions: &[
            "project:read",
            "project:write",
            "agent:run",
            "agent:spawn",
            "deploy:read",
            "observe:read",
            "workspace:read",
        ],
        is_system: false,
    },
    RoleDef {
        name: "otlp-ingest",
        description: "Service account: OTLP telemetry ingestion",
        permissions: &["observe:write", "project:read"],
        is_system: true,
    },
];

// ---------------------------------------------------------------------------
// OTEL service account constants
// ---------------------------------------------------------------------------

/// Well-known user ID for the admin user.
const ADMIN_USER_ID: &str = "00000000-0000-0000-0000-000000000001";

/// Well-known user ID for the OTEL system service account.
const OTEL_USER_ID: &str = "00000000-0000-0000-0000-000000000099";

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let pool = PgPool::connect(&cli.database_url)
        .await
        .context("failed to connect to database")?;

    let kube = kube::Client::try_default()
        .await
        .context("failed to create K8s client")?;

    let namespace = cli
        .namespace
        .unwrap_or_else(|| detect_namespace().unwrap_or_else(|| "platform".into()));

    seed_permissions(&pool).await?;
    seed_roles(&pool).await?;
    seed_admin_user(&pool, &kube, &namespace, &cli.admin_secret_name, cli.dev).await?;
    seed_otel_service_account(&pool, &kube, &namespace, &cli.otel_secret_name).await?;

    tracing::info!("seed complete");
    Ok(())
}

/// Read the in-cluster namespace from the service account mount.
fn detect_namespace() -> Option<String> {
    std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Seed functions
// ---------------------------------------------------------------------------

async fn seed_permissions(pool: &PgPool) -> anyhow::Result<()> {
    let mut value_parts = Vec::with_capacity(PERMISSIONS.len());
    let mut params: Vec<String> = Vec::new();
    for (i, perm) in PERMISSIONS.iter().enumerate() {
        let base = i * 4 + 1;
        value_parts.push(format!(
            "(gen_random_uuid(), ${}, ${}, ${}, ${})",
            base,
            base + 1,
            base + 2,
            base + 3
        ));
        params.push(perm.name.into());
        params.push(perm.resource.into());
        params.push(perm.action.into());
        params.push(perm.description.into());
    }
    let sql = format!(
        "INSERT INTO permissions (id, name, resource, action, description) VALUES {} ON CONFLICT (name) DO NOTHING",
        value_parts.join(", ")
    );
    let mut query = sqlx::query(&sql);
    for p in &params {
        query = query.bind(p);
    }
    query.execute(pool).await?;

    tracing::info!(count = PERMISSIONS.len(), "permissions seeded");
    Ok(())
}

async fn seed_roles(pool: &PgPool) -> anyhow::Result<()> {
    // Insert roles
    {
        let mut value_parts = Vec::with_capacity(ROLES.len());
        for (i, _) in ROLES.iter().enumerate() {
            let base = i * 3 + 1;
            value_parts.push(format!(
                "(gen_random_uuid(), ${}, ${}, ${})",
                base,
                base + 1,
                base + 2,
            ));
        }
        let sql = format!(
            "INSERT INTO roles (id, name, description, is_system) VALUES {} ON CONFLICT (name) DO NOTHING",
            value_parts.join(", ")
        );
        let mut query = sqlx::query(&sql);
        for role in ROLES {
            query = query
                .bind(role.name)
                .bind(role.description)
                .bind(role.is_system);
        }
        query.execute(pool).await?;
    }

    // Insert role-permission mappings
    {
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for role in ROLES {
            let perms: Vec<&str> = if role.name == "admin" {
                PERMISSIONS.iter().map(|p| p.name).collect()
            } else {
                role.permissions.to_vec()
            };
            for perm_name in perms {
                pairs.push((role.name, perm_name));
            }
        }

        if !pairs.is_empty() {
            let mut value_parts = Vec::with_capacity(pairs.len());
            for (i, _) in pairs.iter().enumerate() {
                let base = i * 2 + 1;
                value_parts.push(format!("(${}, ${})", base, base + 1));
            }
            let sql = format!(
                "INSERT INTO role_permissions (role_id, permission_id)
                 SELECT r.id, p.id
                 FROM (VALUES {}) AS v(role_name, perm_name)
                 JOIN roles r ON r.name = v.role_name
                 JOIN permissions p ON p.name = v.perm_name
                 ON CONFLICT DO NOTHING",
                value_parts.join(", ")
            );
            let mut query = sqlx::query(&sql);
            for (role_name, perm_name) in &pairs {
                query = query.bind(*role_name).bind(*perm_name);
            }
            query.execute(pool).await?;
        }
    }

    tracing::info!(count = ROLES.len(), "roles seeded");
    Ok(())
}

async fn seed_admin_user(
    pool: &PgPool,
    kube: &kube::Client,
    namespace: &str,
    secret_name: &str,
    dev_mode: bool,
) -> anyhow::Result<()> {
    let user_id = Uuid::parse_str(ADMIN_USER_ID)?;

    // Check if admin already exists
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = $1)")
        .bind(user_id)
        .fetch_one(pool)
        .await?;

    if exists {
        tracing::info!("admin user already exists, skipping");
        return Ok(());
    }

    // Dev mode: "admin", otherwise random 24-char hex
    let password = if dev_mode {
        "admin".to_string()
    } else {
        let mut bytes = [0u8; 12];
        rand::fill(&mut bytes);
        hex::encode(bytes)
    };

    let password_hash =
        platform_auth::hash_password(&password).context("failed to hash admin password")?;

    sqlx::query(
        "INSERT INTO users (id, name, display_name, email, password_hash)
         VALUES ($1, 'admin', 'Administrator', 'admin@localhost', $2)",
    )
    .bind(user_id)
    .bind(&password_hash)
    .execute(pool)
    .await?;

    // Assign admin role
    sqlx::query(
        "INSERT INTO user_roles (id, user_id, role_id)
         SELECT gen_random_uuid(), $1, r.id FROM roles r WHERE r.name = 'admin'
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .execute(pool)
    .await?;

    // Write password into K8s Secret
    let secret = Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(secret_name.into()),
            namespace: Some(namespace.into()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".into(),
                "platform-seed".into(),
            )])),
            ..Default::default()
        },
        type_: Some("Opaque".into()),
        string_data: Some(BTreeMap::from([
            ("username".into(), "admin".into()),
            ("password".into(), password.clone()),
        ])),
        ..Default::default()
    };

    let secrets_api: Api<Secret> = Api::namespaced(kube.clone(), namespace);
    let patch = serde_json::to_value(&secret)?;
    secrets_api
        .patch(
            secret_name,
            &PatchParams::apply("platform-seed"),
            &Patch::Apply(&patch),
        )
        .await
        .context("failed to create/update admin credentials K8s secret")?;

    if dev_mode {
        tracing::info!("admin user seeded (dev mode: admin/admin)");
    } else {
        tracing::info!(
            namespace,
            secret_name,
            "admin user seeded (password stored in K8s secret)"
        );
    }

    Ok(())
}

async fn seed_otel_service_account(
    pool: &PgPool,
    kube: &kube::Client,
    namespace: &str,
    secret_name: &str,
) -> anyhow::Result<()> {
    let user_id = Uuid::parse_str(OTEL_USER_ID)?;

    // Ensure the service account user exists
    sqlx::query(
        "INSERT INTO users (id, name, display_name, email, password_hash, user_type)
         VALUES ($1, 'otel-system', 'OTEL System Collector', 'otel-system@platform.local', 'nologin', 'service_account')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .execute(pool)
    .await?;

    // Ensure the otlp-ingest role is granted
    sqlx::query(
        "INSERT INTO user_roles (id, user_id, role_id)
         SELECT gen_random_uuid(), $1, r.id FROM roles r WHERE r.name = 'otlp-ingest'
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .execute(pool)
    .await?;

    // Generate a fresh random token
    let (raw_token, token_hash) = platform_auth::generate_api_token();
    let token_id = Uuid::new_v4();

    // Delete any previous OTEL tokens, then insert the new one
    sqlx::query("DELETE FROM api_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;

    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, name, token_hash)
         VALUES ($1, $2, 'otel-infra-collector', $3)",
    )
    .bind(token_id)
    .bind(user_id)
    .bind(&token_hash)
    .execute(pool)
    .await?;

    // Write the raw token into a K8s Secret (server-side apply = idempotent)
    let secret = Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(secret_name.into()),
            namespace: Some(namespace.into()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".into(),
                "platform-seed".into(),
            )])),
            ..Default::default()
        },
        type_: Some("Opaque".into()),
        string_data: Some(BTreeMap::from([("token".into(), raw_token)])),
        ..Default::default()
    };

    let secrets_api: Api<Secret> = Api::namespaced(kube.clone(), namespace);
    let patch = serde_json::to_value(&secret)?;
    secrets_api
        .patch(
            secret_name,
            &PatchParams::apply("platform-seed"),
            &Patch::Apply(&patch),
        )
        .await
        .context("failed to create/update K8s secret")?;

    tracing::info!(
        namespace,
        secret_name,
        "OTEL system service account seeded (role: otlp-ingest, token rotated, K8s secret updated)"
    );
    Ok(())
}
