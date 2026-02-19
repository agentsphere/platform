# 02 — Identity, Auth & RBAC

## Prerequisite
- 01-foundation complete (store, migrations, AppState, bootstrap)

## Blocks
- Every other module depends on auth middleware (`AuthUser` extractor)
- RBAC middleware (`RequirePermission`) used by all API endpoints

## Can Parallelize With
- Nothing — this must complete second, after foundation, before the parallel wave

---

## Scope

User management, authentication (sessions + API tokens), RBAC permission resolution with Valkey caching, delegation system, and tower middleware extractors. Replaces Authelia entirely.

---

## Deliverables

### 1. `src/auth/mod.rs` — Module Root
Re-exports password, token, middleware submodules.

### 2. `src/auth/password.rs` — Argon2id Hashing
- `pub fn hash_password(plain: &str) -> Result<String>` — argon2id with secure defaults
- `pub fn verify_password(plain: &str, hash: &str) -> Result<bool>`
- **IMPORTANT**: Use `argon2::password_hash::rand_core::OsRng` for salt generation, NOT `rand::rng()` or `rand::rngs::OsRng`. Our `rand 0.10` uses `rand_core 0.9` but argon2 needs `rand_core 0.6` — they are incompatible types. See `store/bootstrap.rs` for the working pattern.

### 3. `src/auth/token.rs` — Token Generation
- `pub fn generate_session_token() -> (String, String)` — returns (raw_token, sha256_hash)
- `pub fn generate_api_token() -> (String, String)` — returns (raw_token, sha256_hash)
- Token format: `plat_` prefix + 32 bytes hex (session), `plat_api_` prefix (API token)
- Hash with sha256 for storage — never store raw tokens

### 4. `src/auth/middleware.rs` — Axum Auth Extractor
- `AuthUser` extractor: extracts authenticated user from request
  - Check `Authorization: Bearer <token>` header → look up `api_tokens` by hash
  - Check `session` cookie → look up `auth_sessions` by hash
  - Return `ApiError::Unauthorized` if neither present or expired
  - Update `api_tokens.last_used_at` on successful API token auth
- `OptionalAuthUser` extractor: same but returns `Option<User>` (for public endpoints)

### 5. `src/rbac/mod.rs` — Module Root
Re-exports types, resolver, delegation, middleware.

### 6. `src/rbac/types.rs` — Permission & Role Types
```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    ProjectRead,
    ProjectWrite,
    ProjectDelete,
    AgentRun,
    DeployRead,
    DeployPromote,
    ObserveRead,
    ObserveWrite,
    AlertManage,
    SecretRead,
    SecretWrite,
    AdminUsers,
    AdminDelegate,
}
```

- Permission string format: `resource:action` (e.g., `project:read`)
- `impl Permission { pub fn as_str(&self) -> &str; pub fn from_str(s: &str) -> Result<Self> }` — for DB queries
- **Note**: These must match the 13 permissions seeded by `store/bootstrap.rs`: `project:read`, `project:write`, `project:delete`, `agent:run`, `deploy:read`, `deploy:promote`, `observe:read`, `observe:write`, `alert:manage`, `secret:read`, `secret:write`, `admin:users`, `admin:delegate`
- Don't use `#[sqlx(type_name = "text")]` — permissions are stored as `TEXT` in the `permissions` table, not as a Postgres enum. Parse from the `name` column string.
- Wildcard: admin role gets ALL permissions via `role_permissions` table (already wired by bootstrap), no `*:*` wildcard needed in resolver

### 7. `src/rbac/resolver.rs` — Permission Resolution
- `pub async fn effective_permissions(pool, valkey, user_id, project_id) -> Result<HashSet<Permission>>`
  - Check Valkey cache first: key `perms:{user_id}:{project_id}` (use `store::valkey::get_cached`)
  - On miss: query DB with `sqlx::query_as!` (union of global roles + project roles + active delegations)
  - Cache result with 5min TTL (use `store::valkey::set_cached`)
  - **Note**: After adding `sqlx::query_as!` calls, run `just db-prepare` to update `.sqlx/` offline cache
- `pub async fn has_permission(pool, valkey, user_id, project_id, perm) -> Result<bool>`
- `pub async fn invalidate_permissions(valkey, user_id) -> Result<()>` — called on role/delegation change (use `store::valkey::invalidate`)

### 8. `src/rbac/delegation.rs` — Delegation CRUD
- `pub async fn create_delegation(pool, delegator_id, delegate_id, permission, project_id, expires_at, reason) -> Result<Delegation>`
  - Validate: delegator must hold the permission they're delegating
  - Insert into `delegations` table
  - Invalidate delegate's cached permissions
  - Write audit log entry
- `pub async fn revoke_delegation(pool, delegation_id, actor_id) -> Result<()>`
- `pub async fn list_delegations(pool, user_id) -> Result<Vec<Delegation>>`
  - Both delegations granted by user and delegations received

### 9. `src/rbac/middleware.rs` — Tower Permission Layer
- `RequirePermission` — axum route layer that checks RBAC before handler executes
  - Works with `AuthUser` extractor to get user identity
  - For project-scoped permissions: extracts `project_id` from path parameter named `id` or `project_id`
  - For global permissions (admin routes): checks without project scope
  - Returns `ApiError::Forbidden` if user lacks required permission
- Usage in router: `.route_layer(RequirePermission::new(Permission::ProjectWrite))`
- **Design note**: This is a route layer (not an extractor in the handler signature). The layer runs before the handler, extracts the user from the request extensions (set by auth middleware), extracts project_id from the URL path, and calls `has_permission(user_id, project_id, perm)`. This keeps permission checking out of handler logic.

### 10. `src/api/users.rs` — User Management API
- `POST /api/users` — create user (admin only)
- `GET /api/users` — list users (admin only)
- `GET /api/users/:id` — get user (self or admin)
- `PATCH /api/users/:id` — update user (self or admin)
- `DELETE /api/users/:id` — deactivate user (admin only)
- `POST /api/auth/login` — login with username/password → set session cookie
- `POST /api/auth/logout` — invalidate session
- `GET /api/auth/me` — get current user from session/token

### 11. `src/api/admin.rs` — RBAC Admin API
- `GET /api/admin/roles` — list roles
- `POST /api/admin/roles` — create custom role (admin only)
- `GET /api/admin/roles/:id/permissions` — list role permissions
- `PUT /api/admin/roles/:id/permissions` — set role permissions (admin only)
- `POST /api/admin/users/:id/roles` — assign role to user (admin only)
- `DELETE /api/admin/users/:id/roles/:role_id` — remove role from user
- `POST /api/admin/delegations` — create delegation
- `DELETE /api/admin/delegations/:id` — revoke delegation
- `GET /api/admin/delegations` — list delegations (filtered by user)

### 12. API Token Management
- `POST /api/tokens` — create API token (returns raw token once)
- `GET /api/tokens` — list tokens (name, scopes, last_used — not the raw token)
- `DELETE /api/tokens/:id` — revoke token

### 13. Audit Log Writes
Every mutation in this module writes to `audit_log` (include both `actor_id` and `actor_name`):
- `user.create`, `user.update`, `user.deactivate`
- `role.assign`, `role.remove`, `role.create`
- `delegation.create`, `delegation.revoke`
- `token.create`, `token.revoke`
- `auth.login`, `auth.logout`

---

## Testing

- Unit: password hashing round-trip, token generation format, permission string conversion
- Integration:
  - Create user → login → get session → access protected endpoint
  - Create API token → use bearer auth → access endpoint
  - Assign role → verify permission check passes
  - Delegate permission → verify delegate can access → revoke → verify access denied
  - Expired delegation → verify access denied
  - Project-scoped role → verify cross-project access denied
  - Audit log entries created for all mutations

## Done When

1. Can create users, login, get sessions
2. API token auth works
3. RBAC permission checks enforce access control
4. Delegation flow works end-to-end
5. All auth/RBAC mutations produce audit log entries
6. `AuthUser` and `RequirePermission` extractors work in handler signatures

## Estimated LOC
~1,500 Rust

---

## Foundation Context (from 01-foundation implementation)

Things the implementor must know from Phase 01:

### What already exists
- **`src/store/mod.rs`**: `AppState { pool, valkey, minio, kube, config }` — pass via `State(state): State<AppState>`
- **`src/store/bootstrap.rs`**: Seeds 5 system roles (`admin`, `developer`, `ops`, `agent`, `viewer`), 13 permissions, `role_permissions` wiring, and admin user on first run. The bootstrap uses dynamic `sqlx::query()` — this module should use `sqlx::query_as!()` for compile-time checked queries.
- **`src/store/valkey.rs`**: `get_cached`, `set_cached`, `invalidate`, `publish` helpers
- **`src/store/pool.rs`**: `connect()` + auto-migration
- **`src/error.rs`**: `ApiError` with `NotFound`, `Unauthorized`, `Forbidden`, `BadRequest`, `Conflict`, `Validation`, `ServiceUnavailable`, `Internal` + `From<sqlx::Error>`, `From<fred::error::Error>`, `From<kube::Error>`
- **`src/config.rs`**: `Config` struct with `admin_password: Option<String>`, `master_key: Option<String>`, etc.
- **`src/lib.rs`**: Module stubs `pub mod auth {}` and `pub mod rbac {}` — replace these with real `pub mod auth;` and `pub mod rbac;`
- **`src/main.rs`**: Currently has inline module stubs too — both lib.rs and main.rs need updating when real modules are added

### Crate API gotchas
- **argon2 + rand**: Use `argon2::password_hash::rand_core::OsRng`, NOT `rand::rng()` (rand_core version conflict)
- **fred Pool**: `pool.next()` for `PubsubInterface` methods (Pool doesn't impl it)
- **Dead code**: Remove `#[allow(dead_code)]` from types as they become used (e.g., `ApiError::Unauthorized` will be used by auth middleware)
- **sqlx queries**: After adding `sqlx::query!()` / `sqlx::query_as!()` calls, run `just db-prepare` to update `.sqlx/` cache. Commit `.sqlx/` changes with the code.

### DB schema reference
The full schema is in `plans/unified-platform.md`. Key tables for this module:
- `users` (id, name, email, password_hash, is_active)
- `roles` (id, name, is_system)
- `permissions` (id, name, resource, action)
- `role_permissions` (role_id, permission_id)
- `user_roles` (id, user_id, role_id, project_id, granted_by)
- `delegations` (id, delegator_id, delegate_id, permission_id, project_id, expires_at, revoked_at)
- `auth_sessions` (id, user_id, token_hash, ip_addr, user_agent, expires_at)
- `api_tokens` (id, user_id, name, token_hash, scopes, project_id, last_used_at, expires_at)
- `audit_log` (id, actor_id, actor_name, action, resource, resource_id, project_id, detail, ip_addr)
