use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{password, token};
use crate::rbac::delegation::{self, CreateDelegationParams};
use crate::rbac::types::Permission;

use super::error::AgentError;

/// Result of creating an ephemeral agent identity.
pub struct AgentIdentity {
    pub user_id: Uuid,
    /// Raw API token (shown only once, passed to pod env var).
    pub api_token: String,
}

/// Create an ephemeral agent user, assign the `agent` role, delegate permissions
/// from the requesting user, and generate an API token for the pod.
///
/// `extra_permissions` extends the base set (`ProjectRead` + `ProjectWrite`) with
/// additional capabilities (e.g. `DeployRead`, `DeployPromote` for ops agents).
/// Each permission is silently skipped if the delegator doesn't hold it.
#[tracing::instrument(skip(pool, valkey, extra_permissions), fields(%session_id, %delegator_id, %project_id), err)]
pub async fn create_agent_identity(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    delegator_id: Uuid,
    project_id: Uuid,
    extra_permissions: &[Permission],
) -> Result<AgentIdentity, AgentError> {
    let agent_user_id = Uuid::new_v4();
    let short_id = &session_id.to_string()[..8];
    let agent_name = format!("agent-{short_id}");

    // 1. Create agent user with a random password hash (cannot be used for login)
    let random_hash = password::hash_password(&format!("__agent_nologin_{agent_user_id}__"))
        .map_err(AgentError::Other)?;

    sqlx::query!(
        r#"
        INSERT INTO users (id, name, display_name, email, password_hash, is_active)
        VALUES ($1, $2, $3, $4, $5, true)
        "#,
        agent_user_id,
        agent_name,
        format!("Agent Session {short_id}"),
        format!("{agent_name}@agent.platform.local"),
        random_hash,
    )
    .execute(pool)
    .await?;

    // 2. Assign the "agent" system role
    let role_id = sqlx::query_scalar!("SELECT id FROM roles WHERE name = 'agent'",)
        .fetch_one(pool)
        .await?;

    sqlx::query!(
        "INSERT INTO user_roles (id, user_id, role_id) VALUES ($1, $2, $3)",
        Uuid::new_v4(),
        agent_user_id,
        role_id,
    )
    .execute(pool)
    .await?;

    // 3. Delegate permissions from requesting user to agent user.
    //    Base set: project:read + project:write on the specific project.
    //    Extra permissions (e.g. deploy:read, deploy:promote) are appended.
    //    24-hour hard expiry as a safety net.
    let expires_at = Some(Utc::now() + Duration::hours(24));
    let base_permissions = [Permission::ProjectRead, Permission::ProjectWrite];
    let all_permissions: Vec<&Permission> = base_permissions
        .iter()
        .chain(extra_permissions.iter())
        .collect();

    for perm in &all_permissions {
        // If delegator doesn't hold a permission, create_delegation returns
        // Forbidden — we silently skip (agent gets fewer capabilities).
        let _ = delegation::create_delegation(
            pool,
            valkey,
            &CreateDelegationParams {
                delegator_id,
                delegate_id: agent_user_id,
                permission: **perm,
                project_id: Some(project_id),
                expires_at,
                reason: Some(format!("agent session {session_id}")),
            },
        )
        .await;
    }

    // 4. Generate API token for the agent pod
    let (raw_token, token_hash) = token::generate_api_token();
    let token_expires = Utc::now() + Duration::hours(24);

    sqlx::query!(
        r#"
        INSERT INTO api_tokens (user_id, name, token_hash, scopes, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        agent_user_id,
        format!("agent-session-{session_id}"),
        token_hash,
        &["agent:session".to_owned()][..],
        token_expires,
    )
    .execute(pool)
    .await?;

    tracing::info!(%agent_user_id, %session_id, "agent identity created");

    Ok(AgentIdentity {
        user_id: agent_user_id,
        api_token: raw_token,
    })
}

/// Cleanup an agent identity: revoke delegations, delete tokens, deactivate user.
/// Called when a session finishes (completed, failed, or stopped).
#[tracing::instrument(skip(pool, valkey), fields(%agent_user_id), err)]
pub async fn cleanup_agent_identity(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    agent_user_id: Uuid,
) -> Result<(), AgentError> {
    // Revoke all active delegations where this agent is the delegate
    let delegations = sqlx::query!(
        "SELECT id FROM delegations WHERE delegate_id = $1 AND revoked_at IS NULL",
        agent_user_id,
    )
    .fetch_all(pool)
    .await?;

    for d in &delegations {
        let _ = delegation::revoke_delegation(pool, valkey, d.id).await;
    }

    // Delete all API tokens for this agent user
    sqlx::query!("DELETE FROM api_tokens WHERE user_id = $1", agent_user_id)
        .execute(pool)
        .await?;

    // Delete all auth sessions for this agent user
    sqlx::query!(
        "DELETE FROM auth_sessions WHERE user_id = $1",
        agent_user_id
    )
    .execute(pool)
    .await?;

    // Deactivate the agent user
    sqlx::query!(
        "UPDATE users SET is_active = false WHERE id = $1",
        agent_user_id
    )
    .execute(pool)
    .await?;

    // Invalidate permission cache
    let _ = crate::rbac::resolver::invalidate_permissions(valkey, agent_user_id, None).await;

    tracing::info!(%agent_user_id, "agent identity cleaned up");
    Ok(())
}
