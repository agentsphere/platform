use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};
use sqlx::PgPool;
use uuid::Uuid;

struct RoleDef {
    name: &'static str,
    description: &'static str,
    permissions: &'static [&'static str],
}

const SYSTEM_ROLES: &[RoleDef] = &[
    RoleDef {
        name: "admin",
        description: "Platform administrator with full access",
        permissions: &[], // admin gets all permissions via wildcard logic
    },
    RoleDef {
        name: "developer",
        description: "Human developer with project and agent access",
        permissions: &[
            "project:read",
            "project:write",
            "agent:run",
            "deploy:read",
            "observe:read",
            "secret:read",
        ],
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
        ],
    },
    RoleDef {
        name: "agent",
        description: "AI agent identity — permissions granted via delegation",
        permissions: &[],
    },
    RoleDef {
        name: "viewer",
        description: "Read-only access",
        permissions: &["project:read", "observe:read", "deploy:read"],
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
        name: "admin:delegate",
        resource: "admin",
        action: "delegate",
        description: "Delegate permissions to other users/agents",
    },
];

#[tracing::instrument(skip(pool, admin_password), err)]
pub async fn run(pool: &PgPool, admin_password: Option<&str>) -> anyhow::Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;

    if count > 0 {
        tracing::info!("bootstrap skipped — users already exist");
        return Ok(());
    }

    tracing::info!("first run detected — bootstrapping system data");

    // Insert permissions
    for perm in SYSTEM_PERMISSIONS {
        sqlx::query(
            "INSERT INTO permissions (id, name, resource, action, description)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(perm.name)
        .bind(perm.resource)
        .bind(perm.action)
        .bind(perm.description)
        .execute(pool)
        .await?;
    }

    tracing::info!(count = SYSTEM_PERMISSIONS.len(), "permissions seeded");

    // Insert roles and wire role_permissions
    for role_def in SYSTEM_ROLES {
        let role_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO roles (id, name, description, is_system)
             VALUES ($1, $2, $3, true)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(role_id)
        .bind(role_def.name)
        .bind(role_def.description)
        .execute(pool)
        .await?;

        // admin role gets ALL permissions
        let perms: Vec<&str> = if role_def.name == "admin" {
            SYSTEM_PERMISSIONS.iter().map(|p| p.name).collect()
        } else {
            role_def.permissions.to_vec()
        };

        for perm_name in perms {
            sqlx::query(
                "INSERT INTO role_permissions (role_id, permission_id)
                 SELECT r.id, p.id
                 FROM roles r, permissions p
                 WHERE r.name = $1 AND p.name = $2
                 ON CONFLICT DO NOTHING",
            )
            .bind(role_def.name)
            .bind(perm_name)
            .execute(pool)
            .await?;
        }
    }

    tracing::info!(count = SYSTEM_ROLES.len(), "roles seeded");

    // Create admin user
    let password = admin_password.unwrap_or("admin");
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

    // Assign admin role to admin user
    sqlx::query(
        "INSERT INTO user_roles (id, user_id, role_id)
         SELECT $1, $2, r.id FROM roles r WHERE r.name = 'admin'",
    )
    .bind(Uuid::new_v4())
    .bind(admin_id)
    .execute(pool)
    .await?;

    tracing::info!(user_id = %admin_id, "admin user created");

    Ok(())
}
