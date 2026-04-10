// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

struct RoleDef {
    name: &'static str,
    description: &'static str,
    permissions: &'static [&'static str],
    /// System roles cannot be deleted by admins. Agent roles are `is_system = false`
    /// so admins can customize their permissions.
    is_system: bool,
}

const SYSTEM_ROLES: &[RoleDef] = &[
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
    // Agent-specific roles: is_system=false so admins can customize permissions
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

struct PermDef {
    name: &'static str,
    resource: &'static str,
    action: &'static str,
    description: &'static str,
}

const SYSTEM_PERMISSIONS: &[PermDef] = &[
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

/// Result of the bootstrap process.
pub enum BootstrapResult {
    /// Users already existed — no changes made.
    Skipped,
    /// Dev mode: admin user created with given password.
    DevAdmin,
    /// Production: setup token generated. Caller should log this.
    SetupToken(String),
}

/// Generate a setup token: returns `(raw_hex, sha256_hash_hex)`.
pub fn generate_setup_token() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    let raw = hex::encode(bytes);
    let hash = hash_setup_token(&raw);
    (raw, hash)
}

/// Hash a setup token with SHA-256.
pub fn hash_setup_token(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    hex::encode(digest)
}

/// Bootstrap the platform: seed permissions/roles, then either create admin (dev) or setup token (prod).
#[tracing::instrument(skip(pool, admin_password), err)]
pub async fn run(
    pool: &PgPool,
    admin_password: Option<&str>,
    dev_mode: bool,
) -> anyhow::Result<BootstrapResult> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;

    if count > 0 {
        tracing::info!("bootstrap skipped — users already exist");
        return Ok(BootstrapResult::Skipped);
    }

    tracing::info!("first run detected — bootstrapping system data");

    seed_permissions(pool).await?;
    seed_roles(pool).await?;
    seed_otel_service_account(pool).await?;

    if dev_mode {
        let generated;
        let pw = if let Some(pw) = admin_password {
            pw
        } else {
            let mut bytes = [0u8; 12];
            rand::fill(&mut bytes);
            generated = hex::encode(bytes);
            eprintln!("dev admin password (auto-generated): {generated}");
            &generated
        };
        create_admin_user(pool, pw).await?;
        Ok(BootstrapResult::DevAdmin)
    } else {
        let raw_token = create_setup_token(pool).await?;
        Ok(BootstrapResult::SetupToken(raw_token))
    }
}

async fn seed_permissions(pool: &PgPool) -> anyhow::Result<()> {
    // Batch all permissions into a single multi-row INSERT (1 round-trip instead of 23).
    let mut values = Vec::with_capacity(SYSTEM_PERMISSIONS.len());
    let mut params: Vec<String> = Vec::new();
    for (i, perm) in SYSTEM_PERMISSIONS.iter().enumerate() {
        let base = i * 4 + 1;
        values.push(format!(
            "(gen_random_uuid(), ${}, ${}, ${}, ${})",
            base,
            base + 1,
            base + 2,
            base + 3
        ));
        params.push(perm.name.to_string());
        params.push(perm.resource.to_string());
        params.push(perm.action.to_string());
        params.push(perm.description.to_string());
    }
    let sql = format!(
        "INSERT INTO permissions (id, name, resource, action, description) VALUES {} ON CONFLICT (name) DO NOTHING",
        values.join(", ")
    );
    let mut query = sqlx::query(&sql);
    for p in &params {
        query = query.bind(p);
    }
    query.execute(pool).await?;

    tracing::info!(count = SYSTEM_PERMISSIONS.len(), "permissions seeded");
    Ok(())
}

async fn seed_roles(pool: &PgPool) -> anyhow::Result<()> {
    // Batch all roles into a single multi-row INSERT (1 round-trip instead of 11).
    {
        let mut value_placeholders = Vec::with_capacity(SYSTEM_ROLES.len());
        for (i, _) in SYSTEM_ROLES.iter().enumerate() {
            let base = i * 3 + 1; // 3 bound params per role (name, description, is_system)
            value_placeholders.push(format!(
                "(gen_random_uuid(), ${}, ${}, ${})",
                base,
                base + 1,
                base + 2,
            ));
        }
        let sql = format!(
            "INSERT INTO roles (id, name, description, is_system) VALUES {} ON CONFLICT (name) DO NOTHING",
            value_placeholders.join(", ")
        );
        let mut query = sqlx::query(&sql);
        for role_def in SYSTEM_ROLES {
            query = query
                .bind(role_def.name)
                .bind(role_def.description)
                .bind(role_def.is_system);
        }
        query.execute(pool).await?;
    }

    // Batch all role-permission assignments into a single INSERT ... SELECT (1 round-trip instead of ~70).
    // Build a VALUES list of (role_name, perm_name) pairs, then join against roles and permissions tables.
    {
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for role_def in SYSTEM_ROLES {
            let perms: Vec<&str> = if role_def.name == "admin" {
                SYSTEM_PERMISSIONS.iter().map(|p| p.name).collect()
            } else {
                role_def.permissions.to_vec()
            };
            for perm_name in perms {
                pairs.push((role_def.name, perm_name));
            }
        }

        if !pairs.is_empty() {
            let mut value_placeholders = Vec::with_capacity(pairs.len());
            for (i, _) in pairs.iter().enumerate() {
                let base = i * 2 + 1;
                value_placeholders.push(format!("(${}, ${})", base, base + 1));
            }
            let sql = format!(
                "INSERT INTO role_permissions (role_id, permission_id)
                 SELECT r.id, p.id
                 FROM (VALUES {}) AS v(role_name, perm_name)
                 JOIN roles r ON r.name = v.role_name
                 JOIN permissions p ON p.name = v.perm_name
                 ON CONFLICT DO NOTHING",
                value_placeholders.join(", ")
            );
            let mut query = sqlx::query(&sql);
            for (role_name, perm_name) in &pairs {
                query = query.bind(*role_name).bind(*perm_name);
            }
            query.execute(pool).await?;
        }
    }

    tracing::info!(count = SYSTEM_ROLES.len(), "roles seeded");
    Ok(())
}

/// Seed the OTEL system service account with a deterministic API token.
/// Used by infrastructure OTEL sidecars to push system-level metrics.
async fn seed_otel_service_account(pool: &PgPool) -> anyhow::Result<()> {
    const OTEL_USER_ID: &str = "00000000-0000-0000-0000-000000000099";
    const OTEL_TOKEN_ID: &str = "00000000-0000-0000-0000-0000000000a1";
    const OTEL_RAW_TOKEN: &str = "plat_api_otel_system_dev_000000000000000000000000000000";

    let user_id = Uuid::parse_str(OTEL_USER_ID)?;
    let token_id = Uuid::parse_str(OTEL_TOKEN_ID)?;
    let token_hash = crate::auth::token::hash_token(OTEL_RAW_TOKEN);

    // Service account user
    sqlx::query(
        "INSERT INTO users (id, name, display_name, email, password_hash, user_type)
         VALUES ($1, 'otel-system', 'OTEL System Collector', 'otel-system@platform.local', 'nologin', 'service_account')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .execute(pool)
    .await?;

    // API token (never expires, no project/workspace scope)
    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, name, token_hash)
         VALUES ($1, $2, 'otel-infra-collector', $3)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(token_id)
    .bind(user_id)
    .bind(&token_hash)
    .execute(pool)
    .await?;

    // Grant admin role for system-level metrics (no project_id)
    sqlx::query(
        "INSERT INTO user_roles (id, user_id, role_id)
         SELECT gen_random_uuid(), $1, r.id FROM roles r WHERE r.name = 'admin'
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .execute(pool)
    .await?;

    tracing::info!("OTEL system service account seeded");
    Ok(())
}

/// Create the admin user (dev-mode path).
pub async fn create_admin_user(pool: &PgPool, password: &str) -> anyhow::Result<Uuid> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let password_hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("password hash failed: {e}"))?
        .to_string();

    let admin_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, display_name, email, password_hash)
         VALUES ($1, 'admin', 'Administrator', 'admin@localhost', $2)",
    )
    .bind(admin_id)
    .bind(&password_hash)
    .execute(pool)
    .await?;

    // Assign admin role
    sqlx::query(
        "INSERT INTO user_roles (id, user_id, role_id)
         SELECT $1, $2, r.id FROM roles r WHERE r.name = 'admin'",
    )
    .bind(Uuid::new_v4())
    .bind(admin_id)
    .execute(pool)
    .await?;

    // Create admin's personal workspace
    crate::workspace::service::get_or_create_default_workspace(
        pool,
        admin_id,
        "admin",
        "Administrator",
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to create admin workspace: {e}"))?;

    tracing::info!(user_id = %admin_id, "admin user created");
    Ok(admin_id)
}

/// Generate and store a setup token (production path). Returns the raw token.
async fn create_setup_token(pool: &PgPool) -> anyhow::Result<String> {
    let (raw, hash) = generate_setup_token();

    sqlx::query(
        "INSERT INTO setup_tokens (token_hash, expires_at) VALUES ($1, now() + interval '1 hour')",
    )
    .bind(&hash)
    .execute(pool)
    .await?;

    tracing::info!("setup token generated — use POST /api/setup to create the first admin user");
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_token_format_is_printable() {
        let (raw, _hash) = generate_setup_token();
        // 32 bytes = 64 hex chars
        assert_eq!(raw.len(), 64);
        assert!(raw.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn setup_token_is_sha256_hashed() {
        let (raw, hash) = generate_setup_token();
        assert_ne!(raw, hash);
        assert_eq!(hash, hash_setup_token(&raw));
        // SHA-256 = 32 bytes = 64 hex chars
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn setup_token_hash_deterministic() {
        let hash1 = hash_setup_token("test-token");
        let hash2 = hash_setup_token("test-token");
        assert_eq!(hash1, hash2);
    }
}
