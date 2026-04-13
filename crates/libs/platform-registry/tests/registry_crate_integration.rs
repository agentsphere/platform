// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `platform-registry` crate.
//!
//! Tests exercise gc, access control, and copy_tag against a real Postgres
//! (via `#[sqlx::test]`) with mock trait implementations for permission/workspace checks.

use sqlx::PgPool;
use uuid::Uuid;

use platform_types::{Permission, PermissionChecker, WorkspaceMembershipChecker};

// ---------------------------------------------------------------------------
// Mock trait implementations
// ---------------------------------------------------------------------------

struct AllowAll;
impl PermissionChecker for AllowAll {
    async fn has_permission(
        &self,
        _user_id: Uuid,
        _project_id: Option<Uuid>,
        _perm: Permission,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn has_permission_scoped(
        &self,
        _user_id: Uuid,
        _project_id: Option<Uuid>,
        _perm: Permission,
        _token_scopes: Option<&[String]>,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

struct DenyAll;
impl PermissionChecker for DenyAll {
    async fn has_permission(
        &self,
        _user_id: Uuid,
        _project_id: Option<Uuid>,
        _perm: Permission,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn has_permission_scoped(
        &self,
        _user_id: Uuid,
        _project_id: Option<Uuid>,
        _perm: Permission,
        _token_scopes: Option<&[String]>,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
}

struct AlwaysMember;
impl WorkspaceMembershipChecker for AlwaysMember {
    async fn is_member(&self, _workspace_id: Uuid, _user_id: Uuid) -> anyhow::Result<bool> {
        Ok(true)
    }
}

struct NeverMember;
impl WorkspaceMembershipChecker for NeverMember {
    async fn is_member(&self, _workspace_id: Uuid, _user_id: Uuid) -> anyhow::Result<bool> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Seed helpers
// ---------------------------------------------------------------------------

fn minio_memory() -> opendal::Operator {
    opendal::Operator::new(opendal::services::Memory::default())
        .expect("memory operator")
        .finish()
}

async fn seed_user(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type)
         VALUES ($1, $2, $3, 'not-a-hash', 'human')",
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
    // Add owner as member
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(ws_id)
    .bind(owner_id)
    .execute(pool)
    .await
    .expect("seed workspace member");
    ws_id
}

async fn seed_project(
    pool: &PgPool,
    owner_id: Uuid,
    workspace_id: Uuid,
    name: &str,
    visibility: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    let slug = format!("slug-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO projects (id, owner_id, workspace_id, name, namespace_slug, visibility)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(owner_id)
    .bind(workspace_id)
    .bind(name)
    .bind(&slug)
    .bind(visibility)
    .execute(pool)
    .await
    .expect("seed project");
    id
}

async fn seed_repo(pool: &PgPool, name: &str, project_id: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO registry_repositories (id, project_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(project_id)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed repo");
    id
}

async fn seed_manifest(pool: &PgPool, repo_id: Uuid, digest: &str) {
    sqlx::query(
        "INSERT INTO registry_manifests (repository_id, digest, media_type, content, size_bytes)
         VALUES ($1, $2, 'application/vnd.oci.image.manifest.v1+json', $3, 2)",
    )
    .bind(repo_id)
    .bind(digest)
    .bind(b"{}" as &[u8])
    .execute(pool)
    .await
    .expect("seed manifest");
}

async fn seed_tag(pool: &PgPool, repo_id: Uuid, name: &str, digest: &str) {
    sqlx::query(
        "INSERT INTO registry_tags (repository_id, name, manifest_digest) VALUES ($1, $2, $3)",
    )
    .bind(repo_id)
    .bind(name)
    .bind(digest)
    .execute(pool)
    .await
    .expect("seed tag");
}

fn make_user(
    user_id: Uuid,
    boundary_project_id: Option<Uuid>,
    boundary_workspace_id: Option<Uuid>,
    token_scopes: Option<Vec<String>>,
) -> platform_registry::RegistryUser {
    platform_registry::RegistryUser {
        user_id,
        user_name: "test-user".into(),
        boundary_project_id,
        boundary_workspace_id,
        registry_tag_pattern: None,
        token_scopes,
    }
}

// ===========================================================================
// GC tests
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn gc_no_orphans(pool: PgPool) {
    let minio = minio_memory();
    // No blobs → should be a no-op
    platform_registry::collect_garbage(&pool, &minio)
        .await
        .expect("gc should succeed with no orphans");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn gc_deletes_orphaned_old_blobs(pool: PgPool) {
    let minio = minio_memory();

    // Seed an orphaned blob created > 24h ago
    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    let path = format!("registry/blobs/{digest}");

    // Write blob to storage
    minio
        .write(&path, "blob-data")
        .await
        .expect("write blob to storage");

    // Insert into DB with old created_at
    sqlx::query(
        "INSERT INTO registry_blobs (digest, size_bytes, minio_path, created_at)
         VALUES ($1, 9, $2, now() - interval '48 hours')",
    )
    .bind(&digest)
    .bind(&path)
    .execute(&pool)
    .await
    .expect("seed blob");

    // Verify blob exists
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM registry_blobs WHERE digest = $1")
        .bind(&digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Run GC
    platform_registry::collect_garbage(&pool, &minio)
        .await
        .expect("gc should succeed");

    // Verify blob is deleted from DB
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM registry_blobs WHERE digest = $1")
        .bind(&digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "orphaned blob should be deleted from DB");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn gc_skips_recent_blobs(pool: PgPool) {
    let minio = minio_memory();

    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    let path = format!("registry/blobs/{digest}");

    minio.write(&path, "blob-data").await.expect("write blob");

    // Insert with default created_at (now) — less than 24h grace period
    sqlx::query(
        "INSERT INTO registry_blobs (digest, size_bytes, minio_path)
         VALUES ($1, 9, $2)",
    )
    .bind(&digest)
    .bind(&path)
    .execute(&pool)
    .await
    .expect("seed recent blob");

    platform_registry::collect_garbage(&pool, &minio)
        .await
        .expect("gc should succeed");

    // Blob should still exist (within grace period)
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM registry_blobs WHERE digest = $1")
        .bind(&digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "recent blob should NOT be deleted");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn gc_skips_linked_blobs(pool: PgPool) {
    let minio = minio_memory();

    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    let path = format!("registry/blobs/{digest}");

    minio.write(&path, "data").await.unwrap();

    // Insert old blob
    sqlx::query(
        "INSERT INTO registry_blobs (digest, size_bytes, minio_path, created_at)
         VALUES ($1, 4, $2, now() - interval '48 hours')",
    )
    .bind(&digest)
    .bind(&path)
    .execute(&pool)
    .await
    .expect("seed blob");

    // Create a repo and link the blob
    let repo_id = seed_repo(&pool, &format!("repo-{}", Uuid::new_v4()), None).await;

    sqlx::query("INSERT INTO registry_blob_links (repository_id, blob_digest) VALUES ($1, $2)")
        .bind(repo_id)
        .bind(&digest)
        .execute(&pool)
        .await
        .expect("seed blob link");

    platform_registry::collect_garbage(&pool, &minio)
        .await
        .expect("gc should succeed");

    // Blob should still exist (linked)
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM registry_blobs WHERE digest = $1")
        .bind(&digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "linked blob should NOT be deleted");
}

// ===========================================================================
// Access control: resolve_repo_with_access
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn access_system_repo_pull_ok(pool: PgPool) {
    let repo_name = format!("sys-{}", Uuid::new_v4());
    let repo_id = seed_repo(&pool, &repo_name, None).await;

    let user_id = seed_user(&pool, &format!("u-{}", Uuid::new_v4())).await;
    let user = make_user(user_id, None, None, None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        &repo_name,
        false,
    )
    .await
    .expect("system repo pull should succeed");

    assert_eq!(result.repository_id, repo_id);
    assert!(result.project_id.is_none());
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_system_repo_push_denied(pool: PgPool) {
    let repo_name = format!("sys-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, None).await;

    let user_id = seed_user(&pool, &format!("u-{}", Uuid::new_v4())).await;
    let user = make_user(user_id, None, None, None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        &repo_name,
        true,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::Denied)),
        "push to system repo should be denied"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_owner_has_full_access(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    let repo_id = seed_repo(&pool, &repo_name, Some(project_id)).await;

    let user = make_user(owner_id, None, None, None);

    // Pull
    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &DenyAll,
        &NeverMember,
        &user,
        &repo_name,
        false,
    )
    .await
    .expect("owner pull should succeed even with DenyAll checker");
    assert_eq!(result.repository_id, repo_id);
    assert_eq!(result.project_id, Some(project_id));

    // Push
    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &DenyAll,
        &NeverMember,
        &user,
        &repo_name,
        true,
    )
    .await
    .expect("owner push should succeed even with DenyAll checker");
    assert_eq!(result.repository_id, repo_id);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_non_owner_no_rbac_denied(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let other_id = seed_user(&pool, &format!("other-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    let user = make_user(other_id, None, None, None);

    // DenyAll permission checker + NeverMember → denied
    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &DenyAll,
        &NeverMember,
        &user,
        &repo_name,
        false,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "non-owner without permission should get NameUnknown (404), got: {result:?}"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_workspace_member_implicit_pull(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let member_id = seed_user(&pool, &format!("member-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    let user = make_user(member_id, None, None, None);

    // AlwaysMember → implicit pull access (no RBAC needed)
    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &DenyAll,
        &AlwaysMember,
        &user,
        &repo_name,
        false,
    )
    .await
    .expect("workspace member should have implicit pull");
    assert_eq!(result.project_id, Some(project_id));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_workspace_member_push_needs_rbac(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let member_id = seed_user(&pool, &format!("member-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    let user = make_user(member_id, None, None, None);

    // Workspace member + DenyAll → push requires explicit RBAC
    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &DenyAll,
        &AlwaysMember,
        &user,
        &repo_name,
        true,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "workspace member without push RBAC should be denied"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_non_owner_with_rbac_allowed(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let other_id = seed_user(&pool, &format!("other-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    let user = make_user(other_id, None, None, None);

    // AllowAll → push succeeds
    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &NeverMember,
        &user,
        &repo_name,
        true,
    )
    .await
    .expect("non-owner with RBAC should be allowed to push");
    assert_eq!(result.project_id, Some(project_id));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_boundary_project_mismatch(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    // Token scoped to a DIFFERENT project
    let other_project = Uuid::new_v4();
    let user = make_user(owner_id, Some(other_project), None, None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        &repo_name,
        false,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "project boundary mismatch should return NameUnknown"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_boundary_workspace_mismatch(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    // Token scoped to a DIFFERENT workspace
    let other_ws = Uuid::new_v4();
    let user = make_user(owner_id, None, Some(other_ws), None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        &repo_name,
        false,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "workspace boundary mismatch should return NameUnknown"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_lazy_create_on_push(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    // NO repo exists yet — use project name as repo name for lazy-create
    let user = make_user(owner_id, None, None, None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        &proj_name,
        true,
    )
    .await
    .expect("lazy-create on push should succeed");

    assert_eq!(result.project_id, Some(project_id));

    // Verify repo was actually created
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM registry_repositories WHERE name = $1")
            .bind(&proj_name)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "repo should have been lazy-created");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_lazy_create_namespaced(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    // Namespaced name: "project/dev" → lookup by first segment
    let namespaced = format!("{proj_name}/dev");
    let user = make_user(owner_id, None, None, None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        &namespaced,
        true,
    )
    .await
    .expect("lazy-create with namespaced name should succeed");

    assert_eq!(result.project_id, Some(project_id));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_pull_nonexistent_repo(pool: PgPool) {
    let user_id = seed_user(&pool, &format!("u-{}", Uuid::new_v4())).await;
    let user = make_user(user_id, None, None, None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        "nonexistent-repo",
        false,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "pull from nonexistent repo should return NameUnknown"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn access_push_no_project_returns_name_unknown(pool: PgPool) {
    let user_id = seed_user(&pool, &format!("u-{}", Uuid::new_v4())).await;
    let user = make_user(user_id, None, None, None);

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        &user,
        "no-such-project",
        true,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "push to nonexistent project should return NameUnknown"
    );
}

// ===========================================================================
// Access control: resolve_repo_with_optional_access (anonymous)
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn optional_access_anon_push_unauthorized(pool: PgPool) {
    let result = platform_registry::access::resolve_repo_with_optional_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        None,
        "any-repo",
        true,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::Unauthorized)),
        "anonymous push should return Unauthorized"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn optional_access_anon_pull_system_repo_ok(pool: PgPool) {
    let repo_name = format!("sys-{}", Uuid::new_v4());
    let repo_id = seed_repo(&pool, &repo_name, None).await;

    let result = platform_registry::access::resolve_repo_with_optional_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        None,
        &repo_name,
        false,
    )
    .await
    .expect("anonymous pull from system repo should succeed");

    assert_eq!(result.repository_id, repo_id);
    assert!(result.project_id.is_none());
}

#[sqlx::test(migrations = "../../../migrations")]
async fn optional_access_anon_pull_public_project_ok(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "public").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    let result = platform_registry::access::resolve_repo_with_optional_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        None,
        &repo_name,
        false,
    )
    .await
    .expect("anonymous pull from public project repo should succeed");

    assert_eq!(result.project_id, Some(project_id));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn optional_access_anon_pull_private_unauthorized(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    let result = platform_registry::access::resolve_repo_with_optional_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        None,
        &repo_name,
        false,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::Unauthorized)),
        "anonymous pull from private project should return Unauthorized (not 404)"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn optional_access_anon_pull_nonexistent_repo(pool: PgPool) {
    let result = platform_registry::access::resolve_repo_with_optional_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        None,
        "nonexistent",
        false,
    )
    .await;

    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "anonymous pull from nonexistent repo should return NameUnknown"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn optional_access_authenticated_delegates(pool: PgPool) {
    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    let user = make_user(owner_id, None, None, None);

    let result = platform_registry::access::resolve_repo_with_optional_access(
        &pool,
        &AllowAll,
        &AlwaysMember,
        Some(&user),
        &repo_name,
        false,
    )
    .await
    .expect("authenticated pull should delegate to resolve_repo_with_access");

    assert_eq!(result.project_id, Some(project_id));
}

// ===========================================================================
// copy_tag
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn copy_tag_success(pool: PgPool) {
    let repo_name = format!("repo-{}", Uuid::new_v4());
    let repo_id = seed_repo(&pool, &repo_name, None).await;
    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    seed_manifest(&pool, repo_id, &digest).await;
    seed_tag(&pool, repo_id, "v1", &digest).await;

    platform_registry::copy_tag(&pool, &repo_name, "v1", "v2")
        .await
        .expect("copy_tag should succeed");

    // Verify new tag exists
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM registry_tags WHERE repository_id = $1 AND name = 'v2'",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "destination tag should have been created");

    // Verify it points to the same digest
    let dest_digest: Option<String> = sqlx::query_scalar(
        "SELECT manifest_digest FROM registry_tags WHERE repository_id = $1 AND name = 'v2'",
    )
    .bind(repo_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(dest_digest.as_deref(), Some(digest.as_str()));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn copy_tag_repo_not_found(pool: PgPool) {
    let result = platform_registry::copy_tag(&pool, "no-such-repo", "v1", "v2").await;
    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "copy_tag with nonexistent repo should return NameUnknown"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn copy_tag_source_not_found(pool: PgPool) {
    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, None).await;

    let result = platform_registry::copy_tag(&pool, &repo_name, "no-such-tag", "v2").await;
    assert!(
        matches!(
            result,
            Err(platform_registry::RegistryError::ManifestUnknown)
        ),
        "copy_tag with nonexistent source tag should return ManifestUnknown"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn copy_tag_dest_already_exists(pool: PgPool) {
    let repo_name = format!("repo-{}", Uuid::new_v4());
    let repo_id = seed_repo(&pool, &repo_name, None).await;
    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    seed_manifest(&pool, repo_id, &digest).await;
    seed_tag(&pool, repo_id, "v1", &digest).await;
    seed_tag(&pool, repo_id, "v2", &digest).await;

    let result = platform_registry::copy_tag(&pool, &repo_name, "v1", "v2").await;
    assert!(
        matches!(result, Err(platform_registry::RegistryError::TagExists(ref t)) if t == "v2"),
        "copy_tag to existing tag should return TagExists"
    );
}

// ===========================================================================
// Token scope enforcement
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn access_token_scopes_passed_to_checker(pool: PgPool) {
    // This test verifies that token scopes are forwarded to the permission checker.
    // We use a custom checker that inspects scopes.
    struct ScopeCheckingChecker;
    impl PermissionChecker for ScopeCheckingChecker {
        async fn has_permission(
            &self,
            _user_id: Uuid,
            _project_id: Option<Uuid>,
            _perm: Permission,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn has_permission_scoped(
            &self,
            _user_id: Uuid,
            _project_id: Option<Uuid>,
            _perm: Permission,
            token_scopes: Option<&[String]>,
        ) -> anyhow::Result<bool> {
            // Only allow if scopes include "registry:push"
            Ok(token_scopes
                .map(|s| s.iter().any(|sc| sc == "registry:push"))
                .unwrap_or(false))
        }
    }

    let owner_id = seed_user(&pool, &format!("owner-{}", Uuid::new_v4())).await;
    let other_id = seed_user(&pool, &format!("other-{}", Uuid::new_v4())).await;
    let ws_id = seed_workspace(&pool, owner_id).await;
    let proj_name = format!("proj-{}", Uuid::new_v4());
    let project_id = seed_project(&pool, owner_id, ws_id, &proj_name, "private").await;

    let repo_name = format!("repo-{}", Uuid::new_v4());
    seed_repo(&pool, &repo_name, Some(project_id)).await;

    // User with correct scope
    let user_ok = platform_registry::RegistryUser {
        user_id: other_id,
        user_name: "ok".into(),
        boundary_project_id: None,
        boundary_workspace_id: None,
        registry_tag_pattern: None,
        token_scopes: Some(vec!["registry:push".into()]),
    };

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &ScopeCheckingChecker,
        &NeverMember,
        &user_ok,
        &repo_name,
        true,
    )
    .await;
    assert!(result.is_ok(), "correct scope should allow push");

    // User with wrong scope
    let user_wrong = platform_registry::RegistryUser {
        user_id: other_id,
        user_name: "wrong".into(),
        boundary_project_id: None,
        boundary_workspace_id: None,
        registry_tag_pattern: None,
        token_scopes: Some(vec!["registry:pull".into()]),
    };

    let result = platform_registry::access::resolve_repo_with_access(
        &pool,
        &ScopeCheckingChecker,
        &NeverMember,
        &user_wrong,
        &repo_name,
        true,
    )
    .await;
    assert!(
        matches!(result, Err(platform_registry::RegistryError::NameUnknown)),
        "wrong scope should deny"
    );
}

// ===========================================================================
// Seed tests
// ===========================================================================

/// Create a minimal valid OCI tarball on disk with proper digests.
fn create_test_oci_tarball(path: &std::path::Path) {
    use sha2::{Digest, Sha256};

    let mut builder = tar::Builder::new(Vec::new());

    // oci-layout
    let oci_layout = br#"{"imageLayoutVersion": "1.0.0"}"#;
    let mut header = tar::Header::new_gnu();
    header.set_size(oci_layout.len() as u64);
    header.set_cksum();
    builder
        .append_data(&mut header, "oci-layout", &oci_layout[..])
        .unwrap();

    // Config blob
    let config_content = b"{}";
    let config_hex = hex::encode(Sha256::digest(config_content));
    let mut header = tar::Header::new_gnu();
    header.set_size(config_content.len() as u64);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            format!("blobs/sha256/{config_hex}"),
            &config_content[..],
        )
        .unwrap();

    // Manifest referencing the config
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_hex}"),
            "size": config_content.len()
        },
        "layers": []
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_hex = hex::encode(Sha256::digest(&manifest_bytes));
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            format!("blobs/sha256/{manifest_hex}"),
            &manifest_bytes[..],
        )
        .unwrap();

    // index.json referencing the manifest
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_hex}"),
            "size": manifest_bytes.len()
        }]
    });
    let index_bytes = serde_json::to_vec(&index).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_size(index_bytes.len() as u64);
    header.set_cksum();
    builder
        .append_data(&mut header, "index.json", &index_bytes[..])
        .unwrap();

    builder.finish().unwrap();
    let tar_bytes = builder.into_inner().unwrap();
    std::fs::write(path, tar_bytes).unwrap();
}

#[sqlx::test(migrations = "../../../migrations")]
async fn seed_image_imports_oci_tarball(pool: PgPool) {
    let minio = minio_memory();
    let repo_name = format!("seed-{}", Uuid::new_v4());
    let repo_id = seed_repo(&pool, &repo_name, None).await;

    let dir = tempfile::tempdir().unwrap();
    let tarball = dir.path().join("test.tar");
    create_test_oci_tarball(&tarball);

    let result = platform_registry::seed_image(&pool, &minio, repo_id, &tarball, "v1")
        .await
        .expect("seed_image should succeed");

    match result {
        platform_registry::SeedResult::Imported {
            manifest_digest,
            blob_count,
        } => {
            assert!(manifest_digest.starts_with("sha256:"));
            assert!(blob_count >= 2, "should import config + manifest blobs");
        }
        platform_registry::SeedResult::AlreadyExists => {
            panic!("should not already exist");
        }
    }

    // Verify tag was created in DB
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM registry_tags WHERE repository_id = $1 AND name = 'v1'",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "tag should exist after import");

    // Verify manifest was created
    let manifest_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM registry_manifests WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(manifest_count >= 1, "manifest should be imported");

    // Verify blobs were stored in minio
    let blob_links: i64 =
        sqlx::query_scalar("SELECT count(*) FROM registry_blob_links WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        blob_links >= 2,
        "blob links should exist for config + manifest"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn seed_image_idempotent_returns_already_exists(pool: PgPool) {
    let minio = minio_memory();
    let repo_name = format!("seed-{}", Uuid::new_v4());
    let repo_id = seed_repo(&pool, &repo_name, None).await;

    let dir = tempfile::tempdir().unwrap();
    let tarball = dir.path().join("test.tar");
    create_test_oci_tarball(&tarball);

    // First import
    let result1 = platform_registry::seed_image(&pool, &minio, repo_id, &tarball, "v1")
        .await
        .expect("first seed should succeed");
    assert!(matches!(
        result1,
        platform_registry::SeedResult::Imported { .. }
    ));

    // Second import — same tag → AlreadyExists
    let result2 = platform_registry::seed_image(&pool, &minio, repo_id, &tarball, "v1")
        .await
        .expect("second seed should succeed");
    assert!(matches!(
        result2,
        platform_registry::SeedResult::AlreadyExists
    ));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn seed_all_scans_directory(pool: PgPool) {
    let minio = minio_memory();

    let dir = tempfile::tempdir().unwrap();
    let tarball = dir.path().join("my-image.tar");
    create_test_oci_tarball(&tarball);

    // seed_all scans directory, derives repo name from filename stem
    platform_registry::seed_all(&pool, &minio, dir.path())
        .await
        .expect("seed_all should succeed");

    // Verify repository was auto-created with name "my-image"
    let repo_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM registry_repositories WHERE name = 'my-image'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(repo_count, 1, "repo 'my-image' should be auto-created");

    // Verify tag "v1" was created
    let tag_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM registry_tags t
         JOIN registry_repositories r ON r.id = t.repository_id
         WHERE r.name = 'my-image' AND t.name = 'v1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tag_count, 1, "tag v1 should exist for my-image");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn seed_all_skips_nonexistent_directory(pool: PgPool) {
    let minio = minio_memory();

    // Non-existent path should be a no-op (not an error)
    platform_registry::seed_all(
        &pool,
        &minio,
        std::path::Path::new("/nonexistent/seed/path"),
    )
    .await
    .expect("seed_all with nonexistent path should succeed (no-op)");
}
