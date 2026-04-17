// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use platform_auth::resolver;
use platform_types::validation;
use platform_types::{ApiError, AuthUser, Permission};
use platform_types::{AuditEntry, send_audit};
// TODO: wire from platform crate — slugify_namespace
use platform_k8s::slugify_namespace;

use crate::state::PlatformState;

use super::helpers::{require_admin, require_project_write};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub visibility: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub default_branch: Option<String>,
    pub workspace_id: Option<Uuid>,
    /// Whether to set up K8s namespaces + ops repo. Defaults to `true`.
    /// Set to `false` for DB-only project creation (infra can be set up later
    /// on first deploy/pipeline/agent run via lazy `ensure_namespace()`).
    pub setup_infra: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProjectRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub default_branch: Option<String>,
    pub agent_image: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListProjectsParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub owner_id: Option<Uuid>,
    pub visibility: Option<String>,
    pub search: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectResponse {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: String,
    pub default_branch: String,
    pub namespace_slug: String,
    pub agent_image: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

use platform_types::ListResponse;

/// Insert the project row and return the project ID + `namespace_slug`.
/// Handles unique constraint violations on `namespace_slug` by appending a hash suffix.
#[tracing::instrument(skip(pool, auth, body, repo_path), fields(project_name = %body.name), err)]
async fn insert_project_row(
    pool: &PgPool,
    auth: &AuthUser,
    body: &CreateProjectRequest,
    visibility: &str,
    default_branch: &str,
    repo_path: &str,
    workspace_id: Uuid,
) -> Result<ProjectRow, ApiError> {
    let slug = slugify_namespace(&body.name)?;

    match try_insert_project(
        pool,
        auth,
        body,
        visibility,
        default_branch,
        repo_path,
        workspace_id,
        &slug,
    )
    .await
    {
        Ok(row) => Ok(row),
        Err(ApiError::Conflict(msg)) if msg.contains("namespace") => {
            // Collision on namespace_slug -- append short hash suffix
            let hash = &format!("{:x}", Sha256::digest(body.name.as_bytes()))[..6];
            let slug_with_hash = format!("{}-{hash}", &slug[..slug.len().min(33)]);
            try_insert_project(
                pool,
                auth,
                body,
                visibility,
                default_branch,
                repo_path,
                workspace_id,
                &slug_with_hash,
            )
            .await
        }
        Err(e) => Err(e),
    }
}

// Internal row type to avoid repeating the query_as fields
struct ProjectRow {
    id: Uuid,
    owner_id: Uuid,
    workspace_id: Uuid,
    name: String,
    display_name: Option<String>,
    description: Option<String>,
    visibility: String,
    default_branch: String,
    namespace_slug: String,
    agent_image: Option<String>,
    is_active: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[allow(clippy::too_many_arguments)]
async fn try_insert_project(
    pool: &PgPool,
    auth: &AuthUser,
    body: &CreateProjectRequest,
    visibility: &str,
    default_branch: &str,
    repo_path: &str,
    workspace_id: Uuid,
    namespace_slug: &str,
) -> Result<ProjectRow, ApiError> {
    sqlx::query_as!(
        ProjectRow,
        r#"
        INSERT INTO projects (owner_id, name, display_name, description, visibility, default_branch, repo_path, workspace_id, namespace_slug)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, owner_id, workspace_id, name, display_name, description, visibility, default_branch,
                  namespace_slug, agent_image, is_active, created_at, updated_at
        "#,
        auth.user_id,
        body.name,
        body.display_name,
        body.description,
        visibility,
        default_branch,
        repo_path,
        workspace_id,
        namespace_slug,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505") => {
            if db_err
                .constraint()
                .is_some_and(|c| c.contains("namespace_slug"))
            {
                ApiError::Conflict("namespace slug collision".into())
            } else if db_err
                .constraint()
                .is_some_and(|c| c.contains("owner_id") || c.contains("name"))
            {
                ApiError::Conflict(format!(
                    "a project named '{}' already exists",
                    body.name
                ))
            } else {
                ApiError::from(e)
            }
        }
        _ => ApiError::from(e),
    })
}

/// Set up K8s namespaces, network policy, and ops repo for a new project.
/// Best-effort: failures are logged but do NOT block project creation.
#[tracing::instrument(skip(state), fields(%project_id, %namespace_slug), err)]
pub async fn setup_project_infrastructure(
    state: &PlatformState,
    project_id: Uuid,
    namespace_slug: &str,
) -> Result<(), ApiError> {
    let project_id_str = project_id.to_string();
    let prod_ns = state.config.core.project_namespace(namespace_slug, "prod");

    // 1. Create prod namespace
    let svc_ns = state
        .config
        .core
        .ns_prefix
        .as_deref()
        .unwrap_or(&state.config.core.platform_namespace);
    if let Err(e) = platform_k8s::ensure_namespace_with_services_ns(
        &state.kube,
        &prod_ns,
        "prod",
        &project_id_str,
        &state.config.core.platform_namespace,
        &state.config.gateway.gateway_namespace,
        svc_ns,
        false,
    )
    .await
    {
        tracing::warn!(error = %e, "failed to create prod namespace (will retry)");
    }

    // 4. Auto-create ops repo (best-effort -- don't block project creation)
    match platform_ops_repo::init_ops_repo(
        &state.config.deployer.ops_repos_path,
        namespace_slug,
        "main",
    )
    .await
    {
        Ok(ops_repo_path) => {
            let ops_repo_path_str = ops_repo_path.to_string_lossy().to_string();
            if let Err(e) = sqlx::query!(
                r#"
                INSERT INTO ops_repos (name, repo_path, branch, path, project_id)
                VALUES ($1, $2, 'main', 'deploy/', $3)
                ON CONFLICT (project_id) WHERE project_id IS NOT NULL DO NOTHING
                "#,
                format!("{namespace_slug}-ops"),
                ops_repo_path_str,
                project_id,
            )
            .execute(&state.pool)
            .await
            {
                tracing::warn!(error = %e, "failed to insert ops repo row");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to init ops repo");
        }
    }

    Ok(())
}

fn project_row_to_response(p: ProjectRow) -> ProjectResponse {
    ProjectResponse {
        id: p.id,
        owner_id: p.owner_id,
        workspace_id: p.workspace_id,
        name: p.name,
        display_name: p.display_name,
        description: p.description,
        visibility: p.visibility,
        default_branch: p.default_branch,
        namespace_slug: p.namespace_slug,
        agent_image: p.agent_image,
        is_active: p.is_active,
        created_at: p.created_at,
        updated_at: p.updated_at,
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve workspace: use explicit ID (validating membership) or auto-assign the user's default.
async fn resolve_workspace(
    pool: &PgPool,
    user_id: Uuid,
    owner_name: &str,
    workspace_id: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    if let Some(ws_id) = workspace_id {
        let checker = platform_auth::PgWorkspaceMembershipChecker::new(pool);
        let is_member =
            platform_types::WorkspaceMembershipChecker::is_member(&checker, ws_id, user_id)
                .await
                .map_err(ApiError::Internal)?;
        if !is_member {
            return Err(ApiError::BadRequest(
                "you are not a member of this workspace".into(),
            ));
        }
        Ok(ws_id)
    } else {
        // Get or create default workspace for user
        let existing = sqlx::query_scalar!(
            r#"SELECT id FROM workspaces WHERE owner_id = $1 AND is_active = true ORDER BY created_at LIMIT 1"#,
            user_id,
        )
        .fetch_optional(pool)
        .await?;

        if let Some(id) = existing {
            return Ok(id);
        }

        let ws_id = Uuid::new_v4();
        let ws_name = format!("{owner_name}-personal");
        let ws_display = format!("{owner_name}'s workspace");

        let mut tx = pool.begin().await?;
        sqlx::query!(
            r#"INSERT INTO workspaces (id, name, display_name, description, owner_id)
               VALUES ($1, $2, $3, 'Personal workspace', $4)"#,
            ws_id,
            ws_name,
            ws_display,
            user_id,
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

        sqlx::query!(
            "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
            ws_id,
            user_id,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(ws_id)
    }
}

/// Validate inputs for creating a project.
fn validate_create_inputs(body: &CreateProjectRequest) -> Result<(), ApiError> {
    validation::check_name(&body.name)?;
    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }
    if let Some(ref branch) = body.default_branch {
        validation::check_branch_name(branch)?;
    }
    let visibility = body.visibility.as_deref().unwrap_or("private");
    if !["private", "internal", "public"].contains(&visibility) {
        return Err(ApiError::BadRequest(
            "visibility must be private, internal, or public".into(),
        ));
    }
    Ok(())
}

/// Initialize a bare git repo and resolve the workspace for a new project.
async fn init_project_repo_and_workspace(
    state: &PlatformState,
    auth: &AuthUser,
    body: &CreateProjectRequest,
) -> Result<(String, Uuid), ApiError> {
    let default_branch = body.default_branch.as_deref().unwrap_or("main");

    let owner_name = sqlx::query_scalar!("SELECT name FROM users WHERE id = $1", auth.user_id)
        .fetch_one(&state.pool)
        .await?;

    let git_mgr = platform_git::CliGitRepoManager;
    let repo_path = platform_git::traits::GitRepoManager::init_bare(
        &git_mgr,
        &state.config.git.git_repos_path,
        &owner_name,
        &body.name,
        default_branch,
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let repo_path_str = repo_path.to_string_lossy().to_string();

    // Workspace-scoped tokens must create projects in their workspace
    let requested_workspace_id = if let Some(scope_wid) = auth.boundary_workspace_id {
        if body.workspace_id.is_some() && body.workspace_id != Some(scope_wid) {
            return Err(ApiError::BadRequest(
                "workspace_id does not match token scope".into(),
            ));
        }
        Some(scope_wid)
    } else {
        body.workspace_id
    };

    let workspace_id = resolve_workspace(
        &state.pool,
        auth.user_id,
        &owner_name,
        requested_workspace_id,
    )
    .await?;

    Ok((repo_path_str, workspace_id))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(project_name = %body.name), err)]
async fn create_project(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<CreateProjectRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Rate limit: 10 project creations per hour per user
    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "project_create",
        &auth.user_id.to_string(),
        10,
        3600,
    )
    .await?;

    // Project-scoped tokens cannot create new projects
    if auth.boundary_project_id.is_some() {
        return Err(ApiError::Forbidden);
    }

    // Require project:write globally or admin
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ProjectWrite,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

    validate_create_inputs(&body)?;

    let visibility = body.visibility.as_deref().unwrap_or("private");
    let default_branch = body.default_branch.as_deref().unwrap_or("main");

    let (repo_path_str, workspace_id) =
        init_project_repo_and_workspace(&state, &auth, &body).await?;

    let project = insert_project_row(
        &state.pool,
        &auth,
        &body,
        visibility,
        default_branch,
        &repo_path_str,
        workspace_id,
    )
    .await?;

    // Best-effort infra setup (namespaces, network policy, ops repo)
    if body.setup_infra.unwrap_or(true)
        && let Err(e) =
            setup_project_infrastructure(&state, project.id, &project.namespace_slug).await
    {
        tracing::warn!(error = %e, project_id = %project.id, "project infra setup incomplete");
    }

    // Auto-create default branch protection rule for the default branch
    if let Err(e) = sqlx::query!(
        r#"INSERT INTO branch_protection_rules (project_id, pattern) VALUES ($1, $2)
           ON CONFLICT (project_id, pattern) DO NOTHING"#,
        project.id,
        default_branch,
    )
    .execute(&state.pool)
    .await
    {
        tracing::warn!(error = %e, project_id = %project.id, "failed to create default branch protection");
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "project.create".into(),
            resource: "project".into(),
            resource_id: Some(project.id),
            project_id: Some(project.id),
            detail: Some(serde_json::json!({"name": body.name, "visibility": visibility})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(project_row_to_response(project))))
}

async fn list_projects(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Query(params): Query<ListProjectsParams>,
) -> Result<Json<ListResponse<ProjectResponse>>, ApiError> {
    let limit = params.limit.unwrap_or(50).clamp(0, 100);
    let offset = params.offset.unwrap_or(0).max(0);
    let search_pattern = params.search.as_deref().map(|s| {
        let escaped = s.replace('%', "\\%").replace('_', "\\_");
        format!("%{escaped}%")
    });

    // Count matching projects visible to the user
    // A30: include RBAC-granted and workspace-member visibility for private projects
    // A10: enforce token boundary_project_id and boundary_workspace_id
    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!: i64"
        FROM projects
        WHERE is_active = true
          AND ($1::uuid IS NULL OR owner_id = $1)
          AND ($2::text IS NULL OR visibility = $2)
          AND ($3::text IS NULL OR name ILIKE $3)
          AND ($5::uuid IS NULL OR id = $5)
          AND ($6::uuid IS NULL OR workspace_id = $6)
          AND (
              visibility = 'public'
              OR visibility = 'internal'
              OR owner_id = $4
              OR EXISTS(
                  SELECT 1 FROM user_roles ur
                  JOIN role_permissions rp ON rp.role_id = ur.role_id
                  JOIN permissions p ON p.id = rp.permission_id
                  WHERE ur.user_id = $4 AND p.name = 'project:read'
                  AND (ur.project_id = projects.id OR ur.project_id IS NULL)
              )
              OR EXISTS(
                  SELECT 1 FROM workspace_members wm
                  WHERE wm.workspace_id = projects.workspace_id
                  AND wm.user_id = $4
              )
          )
        "#,
        params.owner_id,
        params.visibility,
        search_pattern,
        auth.user_id,
        auth.boundary_project_id,
        auth.boundary_workspace_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query_as!(
        ProjectRow,
        r#"
        SELECT id, owner_id, workspace_id, name, display_name, description, visibility, default_branch,
               namespace_slug, agent_image, is_active, created_at, updated_at
        FROM projects
        WHERE is_active = true
          AND ($1::uuid IS NULL OR owner_id = $1)
          AND ($2::text IS NULL OR visibility = $2)
          AND ($3::text IS NULL OR name ILIKE $3)
          AND ($5::uuid IS NULL OR id = $5)
          AND ($6::uuid IS NULL OR workspace_id = $6)
          AND (
              visibility = 'public'
              OR visibility = 'internal'
              OR owner_id = $4
              OR EXISTS(
                  SELECT 1 FROM user_roles ur
                  JOIN role_permissions rp ON rp.role_id = ur.role_id
                  JOIN permissions p ON p.id = rp.permission_id
                  WHERE ur.user_id = $4 AND p.name = 'project:read'
                  AND (ur.project_id = projects.id OR ur.project_id IS NULL)
              )
              OR EXISTS(
                  SELECT 1 FROM workspace_members wm
                  WHERE wm.workspace_id = projects.workspace_id
                  AND wm.user_id = $4
              )
          )
        ORDER BY created_at DESC
        LIMIT $7 OFFSET $8
        "#,
        params.owner_id,
        params.visibility,
        search_pattern,
        auth.user_id,
        auth.boundary_project_id,
        auth.boundary_workspace_id,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows.into_iter().map(project_row_to_response).collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_project(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ProjectResponse>, ApiError> {
    // Enforce hard project scope from API token
    auth.check_project_scope(id)?;

    let project = sqlx::query_as!(
        ProjectRow,
        r#"
        SELECT id, owner_id, workspace_id, name, display_name, description, visibility, default_branch,
               namespace_slug, agent_image, is_active, created_at, updated_at
        FROM projects WHERE id = $1 AND is_active = true
        "#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    // Enforce hard workspace scope from API token
    if let Some(scope_wid) = auth.boundary_workspace_id
        && project.workspace_id != scope_wid
    {
        return Err(ApiError::NotFound("project".into()));
    }

    // Visibility check: private projects only visible to owner or those with project:read
    if project.visibility == "private" && project.owner_id != auth.user_id {
        let allowed = resolver::has_permission_scoped(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(id),
            Permission::ProjectRead,
            auth.token_scopes.as_deref(),
        )
        .await
        .map_err(ApiError::Internal)?;

        if !allowed {
            return Err(ApiError::NotFound("project".into()));
        }
    }

    Ok(Json(project_row_to_response(project)))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn update_project(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateProjectRequest>,
) -> Result<Json<ProjectResponse>, ApiError> {
    // Enforce hard project scope from API token
    auth.check_project_scope(id)?;

    // A1: Always check RBAC -- owner gets implicit write via workspace-derived permissions
    require_project_write(&state, &auth, id).await?;

    // Validate inputs
    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }
    if let Some(ref branch) = body.default_branch {
        validation::check_branch_name(branch)?;
    }

    if let Some(ref vis) = body.visibility
        && !["private", "internal", "public"].contains(&vis.as_str())
    {
        return Err(ApiError::BadRequest(
            "visibility must be private, internal, or public".into(),
        ));
    }
    if let Some(ref image) = body.agent_image {
        validation::check_pipeline_image(image)?;
    }

    let project = sqlx::query_as!(
        ProjectRow,
        r#"
        UPDATE projects SET
            display_name = COALESCE($2, display_name),
            description = COALESCE($3, description),
            visibility = COALESCE($4, visibility),
            default_branch = COALESCE($5, default_branch),
            agent_image = COALESCE($6, agent_image),
            updated_at = now()
        WHERE id = $1 AND is_active = true
        RETURNING id, owner_id, workspace_id, name, display_name, description, visibility, default_branch,
                  namespace_slug, agent_image, is_active, created_at, updated_at
        "#,
        id,
        body.display_name,
        body.description,
        body.visibility,
        body.default_branch,
        body.agent_image,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "project.update".into(),
            resource: "project".into(),
            resource_id: Some(id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(project_row_to_response(project)))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn delete_project(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // Enforce hard project scope from API token
    auth.check_project_scope(id)?;

    // A1: Only admins can delete projects -- no owner bypass
    require_admin(&state, &auth).await?;

    // Verify project exists
    let _exists = sqlx::query_scalar!(
        "SELECT id FROM projects WHERE id = $1 AND is_active = true",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    // Soft-delete
    sqlx::query!("UPDATE projects SET is_active = false WHERE id = $1", id)
        .execute(&state.pool)
        .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "project.delete".into(),
            resource: "project".into(),
            resource_id: Some(id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}
