use sqlx::PgPool;
use uuid::Uuid;

use super::types::{Workspace, WorkspaceMember};
use crate::error::ApiError;

/// Check whether a user is a member of a workspace (any role).
pub async fn is_member(pool: &PgPool, workspace_id: Uuid, user_id: Uuid) -> Result<bool, ApiError> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(
            SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND user_id = $2
        ) as "exists!: bool""#,
        workspace_id,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Check whether a user has at least admin role in a workspace.
pub async fn is_admin(pool: &PgPool, workspace_id: Uuid, user_id: Uuid) -> Result<bool, ApiError> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(
            SELECT 1 FROM workspace_members
            WHERE workspace_id = $1 AND user_id = $2 AND role IN ('owner', 'admin')
        ) as "exists!: bool""#,
        workspace_id,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Check whether a user is the workspace owner.
pub async fn is_owner(pool: &PgPool, workspace_id: Uuid, user_id: Uuid) -> Result<bool, ApiError> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(
            SELECT 1 FROM workspaces WHERE id = $1 AND owner_id = $2 AND is_active = true
        ) as "exists!: bool""#,
        workspace_id,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Get a workspace by ID.
pub async fn get_workspace(pool: &PgPool, id: Uuid) -> Result<Option<Workspace>, ApiError> {
    let row = sqlx::query_as!(
        Workspace,
        r#"SELECT id, name, display_name, description, owner_id, is_active,
                  created_at, updated_at
           FROM workspaces WHERE id = $1 AND is_active = true"#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// List workspaces the user is a member of.
pub async fn list_user_workspaces(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Workspace>, i64), ApiError> {
    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64"
           FROM workspaces w
           JOIN workspace_members wm ON wm.workspace_id = w.id
           WHERE wm.user_id = $1 AND w.is_active = true"#,
        user_id,
    )
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query_as!(
        Workspace,
        r#"SELECT w.id, w.name, w.display_name, w.description, w.owner_id,
                  w.is_active, w.created_at, w.updated_at
           FROM workspaces w
           JOIN workspace_members wm ON wm.workspace_id = w.id
           WHERE wm.user_id = $1 AND w.is_active = true
           ORDER BY w.updated_at DESC
           LIMIT $2 OFFSET $3"#,
        user_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    Ok((rows, total))
}

/// Create a workspace and add the creator as owner.
///
/// Uses a transaction to ensure both the workspace INSERT and the owner
/// membership INSERT succeed or fail atomically.
pub async fn create_workspace(
    pool: &PgPool,
    owner_id: Uuid,
    name: &str,
    display_name: Option<&str>,
    description: Option<&str>,
) -> Result<Workspace, ApiError> {
    let id = Uuid::new_v4();

    let mut tx = pool.begin().await?;

    sqlx::query!(
        r#"INSERT INTO workspaces (id, name, display_name, description, owner_id)
           VALUES ($1, $2, $3, $4, $5)"#,
        id,
        name,
        display_name,
        description,
        owner_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref db_err) = e
            && db_err.constraint() == Some("workspaces_name_key")
        {
            return ApiError::Conflict("workspace name already taken".into());
        }
        ApiError::from(e)
    })?;

    // Add owner as 'owner' member
    sqlx::query!(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
        id,
        owner_id,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    get_workspace(pool, id)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("workspace vanished after creation")))
}

/// Update a workspace.
pub async fn update_workspace(
    pool: &PgPool,
    id: Uuid,
    display_name: Option<&str>,
    description: Option<&str>,
) -> Result<Workspace, ApiError> {
    let result = sqlx::query!(
        r#"UPDATE workspaces SET
            display_name = COALESCE($2, display_name),
            description = COALESCE($3, description),
            updated_at = now()
           WHERE id = $1 AND is_active = true"#,
        id,
        display_name,
        description,
    )
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("workspace".into()));
    }

    get_workspace(pool, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("workspace".into()))
}

/// Soft-delete a workspace.
pub async fn delete_workspace(pool: &PgPool, id: Uuid) -> Result<bool, ApiError> {
    let result = sqlx::query!(
        "UPDATE workspaces SET is_active = false, updated_at = now() WHERE id = $1 AND is_active = true",
        id,
    )
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        // A40: Cascade soft-delete to workspace projects
        sqlx::query(
            "UPDATE projects SET is_active = false, updated_at = now() WHERE workspace_id = $1 AND is_active = true",
        )
        .bind(id)
        .execute(pool)
        .await?;
    }

    Ok(result.rows_affected() > 0)
}

/// List members of a workspace.
pub async fn list_members(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<WorkspaceMember>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT wm.id, wm.workspace_id, wm.user_id, u.name as user_name,
                  wm.role, wm.created_at
           FROM workspace_members wm
           JOIN users u ON u.id = wm.user_id
           WHERE wm.workspace_id = $1
           ORDER BY wm.created_at"#,
        workspace_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| WorkspaceMember {
            id: r.id,
            workspace_id: r.workspace_id,
            user_id: r.user_id,
            user_name: r.user_name,
            role: r.role,
            created_at: r.created_at,
        })
        .collect())
}

/// Add a member to a workspace.
pub async fn add_member(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    role: &str,
) -> Result<(), ApiError> {
    sqlx::query!(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        workspace_id,
        user_id,
        role,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a member from a workspace.
pub async fn remove_member(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
) -> Result<bool, ApiError> {
    let result = sqlx::query!(
        "DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
        workspace_id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Build the default workspace name for a user: `"{username}-personal"`.
pub fn default_workspace_name(username: &str) -> String {
    format!("{username}-personal")
}

/// Build the default workspace display name: `"{display_name}'s workspace"`.
pub fn default_workspace_display_name(display_name: &str) -> String {
    format!("{display_name}'s workspace")
}

/// Get or create a user's default (personal) workspace. Idempotent.
///
/// Looks for an existing workspace owned by the user with the `-personal` suffix.
/// If none exists, creates one and adds the user as owner member.
pub async fn get_or_create_default_workspace(
    pool: &PgPool,
    user_id: Uuid,
    username: &str,
    display_name: &str,
) -> Result<Uuid, ApiError> {
    // Check for existing workspace owned by this user
    let existing = sqlx::query_scalar!(
        r#"SELECT id as "id: Uuid" FROM workspaces
           WHERE owner_id = $1 AND is_active = true
           ORDER BY created_at LIMIT 1"#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;

    if let Some(id) = existing {
        return Ok(id);
    }

    // Create personal workspace
    let ws_name = default_workspace_name(username);
    let ws_display = default_workspace_display_name(display_name);
    let ws = create_workspace(
        pool,
        user_id,
        &ws_name,
        Some(&ws_display),
        Some("Personal workspace"),
    )
    .await?;
    Ok(ws.id)
}

/// Get workspace role for a user. Returns None if not a member.
#[allow(dead_code)]
pub async fn get_member_role(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
) -> Result<Option<String>, ApiError> {
    let row = sqlx::query_scalar!(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
        workspace_id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_name_format() {
        assert_eq!(default_workspace_name("alice"), "alice-personal");
        assert_eq!(default_workspace_name("admin"), "admin-personal");
    }

    #[test]
    fn default_workspace_display_name_format() {
        assert_eq!(
            default_workspace_display_name("Alice Smith"),
            "Alice Smith's workspace"
        );
        assert_eq!(
            default_workspace_display_name("Administrator"),
            "Administrator's workspace"
        );
    }

    #[test]
    fn default_workspace_name_with_special_chars() {
        assert_eq!(
            default_workspace_name("user-with-dashes"),
            "user-with-dashes-personal"
        );
        assert_eq!(
            default_workspace_name("user_underscore"),
            "user_underscore-personal"
        );
    }

    #[test]
    fn default_workspace_display_name_empty() {
        assert_eq!(default_workspace_display_name(""), "'s workspace");
    }

    #[test]
    fn default_workspace_name_empty() {
        assert_eq!(default_workspace_name(""), "-personal");
    }

    #[test]
    fn default_workspace_display_name_with_apostrophe() {
        assert_eq!(
            default_workspace_display_name("O'Brien"),
            "O'Brien's workspace"
        );
    }
}
