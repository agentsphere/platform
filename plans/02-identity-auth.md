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
- Use `argon2` crate with `rand` for salt generation

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
#[derive(Debug, Clone, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Permission {
    ProjectRead,
    ProjectWrite,
    AgentRun,
    DeployRead,
    DeployPromote,
    ObserveRead,
    ObserveWrite,
    AlertManage,
    SecretRead,
    SecretWrite,
    AdminUsers,
    AdminRoles,
}
```

- Permission string format: `resource:action` (e.g., `project:read`)
- `impl Permission { pub fn as_str(&self) -> &str }` — for DB queries
- Wildcard: admin role has `*:*` — checked in resolver

### 7. `src/rbac/resolver.rs` — Permission Resolution
- `pub async fn effective_permissions(pool, valkey, user_id, project_id) -> Result<HashSet<Permission>>`
  - Check Valkey cache first: key `perms:{user_id}:{project_id}`
  - On miss: query DB (union of global roles + project roles + active delegations)
  - Cache result with 5min TTL
- `pub async fn has_permission(pool, valkey, user_id, project_id, perm) -> Result<bool>`
- `pub async fn invalidate_permissions(valkey, user_id) -> Result<()>` — called on role/delegation change

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
- `RequirePermission` — tower layer that checks RBAC before handler executes
  - Works with `AuthUser` extractor to get user identity
  - Extracts `project_id` from path parameters when checking project-scoped permissions
  - Returns `ApiError::Forbidden` if user lacks required permission
- Usage in router: `.route_layer(RequirePermission::new(Permission::ProjectWrite))`

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
Every mutation in this module writes to `audit_log`:
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
