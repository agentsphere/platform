# 03 — Git Server

## Prerequisite
- 01-foundation complete (store, AppState)
- 02-identity-auth complete (AuthUser extractor, RequirePermission middleware)

## Blocks
- 05-build-engine (pipeline triggers on git push via pre-receive hook)
- 04-project-mgmt (file browser reads from git repos)

## Can Parallelize With
- 04-project-mgmt (if file browser deferred to end)
- 05-build-engine, 06-deployer, 07-agent, 08-observability, 09-secrets-notify

---

## Scope

Lightweight git hosting over smart HTTP protocol. Bare repos on disk, metadata in Postgres. Pre-receive hooks for auth and pipeline triggers. File/tree browsing API for UI and agents. LFS batch API redirecting to MinIO. Replaces Gitea's core git hosting.

---

## Deliverables

### 1. `src/git/mod.rs` — Module Root
Re-exports smart_http, hooks, browser, lfs.

### 2. `src/git/smart_http.rs` — Git Smart HTTP Protocol

Implements the two git smart HTTP endpoints that `git clone` and `git push` use:

- `GET /:owner/:repo/info/refs?service=git-upload-pack` — ref advertisement (clone/fetch)
- `GET /:owner/:repo/info/refs?service=git-receive-pack` — ref advertisement (push)
- `POST /:owner/:repo/git-upload-pack` — pack negotiation (clone/fetch)
- `POST /:owner/:repo/git-receive-pack` — receive pushed data

Implementation:
- Extract `owner` and `repo` from path
- Look up project in DB → get `repo_path` (or derive from `{git_repos_path}/{owner}/{repo}.git`)
- Auth: extract credentials from HTTP Basic Auth header → validate against `users` table or API token
- RBAC: `project:read` for upload-pack, `project:write` for receive-pack
- Shell out to `git upload-pack --stateless-rpc` / `git receive-pack --stateless-rpc` via `tokio::process::Command`
- Stream stdin/stdout between HTTP body and git process
- Set correct `Content-Type` headers (`application/x-git-upload-pack-result`, etc.)

### 3. `src/git/hooks.rs` — Pre-receive Hook

Server-side pre-receive hook logic (called during receive-pack, not as a git hook script):

- Parse the pushed refs from receive-pack output (old-sha new-sha refname)
- Validate: reject force-push to default branch unless user has admin role
- On successful push to any branch:
  - If `.platform.yaml` exists in the repo → queue pipeline trigger (write to `pipelines` table with status `pending`)
  - Fire webhook events for `push` subscribers

### 4. `src/git/browser.rs` — Repository Browser API

REST API for browsing git repo contents (used by UI and agents):

- `GET /api/projects/:id/tree?ref=main&path=/` — list directory contents
  - Returns: `[{name, type (blob/tree), size, mode}]`
  - Shell out: `git ls-tree <ref> <path>`
- `GET /api/projects/:id/blob?ref=main&path=src/main.rs` — read file contents
  - Returns: file content as text or base64 for binary
  - Shell out: `git show <ref>:<path>`
- `GET /api/projects/:id/branches` — list branches
  - Shell out: `git branch --format='%(refname:short) %(objectname:short) %(creatordate:iso-strict)'`
- `GET /api/projects/:id/commits?ref=main&limit=20` — list recent commits
  - Shell out: `git log --format=json <ref> -n <limit>`

All endpoints require `project:read` permission.

### 5. `src/git/lfs.rs` — Git LFS Batch API

Minimal LFS implementation that redirects storage to MinIO:

- `POST /:owner/:repo/info/lfs/objects/batch` — LFS batch API
  - For upload operations: return presigned MinIO PUT URLs
  - For download operations: return presigned MinIO GET URLs
  - Object path in MinIO: `lfs/{project_id}/{oid}`
- Auth: LFS uses the same HTTP Basic Auth as smart HTTP

### 6. Repo Initialization

When a project is created (in 04-project-mgmt), initialize a bare git repo:
- `git init --bare {git_repos_path}/{owner}/{name}.git`
- Set `HEAD` to `refs/heads/{default_branch}`
- Store `repo_path` in `projects` table

Provide a helper function callable from the projects API:
- `pub async fn init_bare_repo(repos_path: &Path, owner: &str, name: &str, default_branch: &str) -> Result<PathBuf>`

---

## API Routes Summary

```
# Git smart HTTP (not under /api — git clients expect root-level paths)
GET  /:owner/:repo/info/refs
POST /:owner/:repo/git-upload-pack
POST /:owner/:repo/git-receive-pack

# LFS
POST /:owner/:repo/info/lfs/objects/batch

# Browser API
GET  /api/projects/:id/tree
GET  /api/projects/:id/blob
GET  /api/projects/:id/branches
GET  /api/projects/:id/commits
```

---

## Testing

- Unit: ref parsing, path validation, content-type headers
- Integration:
  - Init bare repo → clone (empty) → push a commit → clone again → verify content
  - Push without auth → 401
  - Push without project:write → 403
  - Browse tree/blob after push → correct file listing and content
  - LFS batch API returns valid presigned URLs
  - Pre-receive hook triggers pipeline row insertion

## Done When

1. `git clone http://localhost:8080/user/repo` works
2. `git push` works with auth (Basic Auth via username + API token as password)
3. RBAC enforced on push/clone
4. File browser API returns correct tree/blob content
5. Push creates pipeline trigger (row in `pipelines` table)
6. LFS batch API returns MinIO presigned URLs

## Estimated LOC
~1,400 Rust

---

## Foundation & Auth Context (from 01+02 implementation)

Things the implementor must know from completed phases:

### What already exists
- **`src/store::AppState`** — `{ pool: PgPool, valkey: fred::clients::Pool, minio: opendal::Operator, kube: kube::Client, config: Arc<Config> }`
- **`src/config::Config`** — includes `git_repos_path: PathBuf` (default `/data/repos`), `minio_endpoint`, etc.
- **`src/auth/middleware::AuthUser`** — axum `FromRequestParts` extractor. Fields: `user_id: Uuid`, `user_name: String`, `ip_addr: Option<String>`. Checks Bearer token then session cookie.
- **`src/auth/token::hash_token`** — `fn hash_token(token: &str) -> String` — SHA-256 hex digest. Use this to look up API tokens by hash.
- **`src/rbac::Permission`** — enum with 13 variants including `ProjectRead`, `ProjectWrite`. `as_str(self)` returns `"project:read"`, etc. (takes `self` by value, not `&self` — it's `Copy`).
- **`src/rbac::resolver`** — `has_permission(pool, valkey, user_id, project_id, perm) -> Result<bool>`, `invalidate_permissions(valkey, user_id, project_id) -> Result<()>`
- **`src/rbac::middleware::require_permission`** — route-layer middleware. Usage: `.route_layer(axum::middleware::from_fn_with_state(state, require_permission(Permission::ProjectRead)))`. Extracts `project_id` from `/projects/:id` path segments automatically.
- **`src/error::ApiError`** — `NotFound(String)`, `Unauthorized`, `Forbidden`, `BadRequest(String)`, `Conflict(String)`, `Internal(anyhow::Error)`. Implements `From<sqlx::Error>`, `From<fred::error::Error>`.
- **`src/api/mod.rs`** — `pub fn router() -> Router<AppState>` that merges `users::router()` and `admin::router()`. New modules should add their router here or be merged in `main.rs`.

### Router pattern
Each module exposes `pub fn router() -> Router<AppState>`. The git module will need two routers:
1. API routes under `/api/projects/:id/*` — merged into the main api router
2. Smart HTTP routes at root level (`/:owner/:repo/*`) — merged directly in `main.rs`

### Auth for git smart HTTP
Git clients use HTTP Basic Auth. The `AuthUser` extractor checks `Authorization: Bearer` and session cookies, but NOT Basic Auth. The git module needs its own Basic Auth extraction:
- Username → look up user by name
- Password → could be account password or API token. For API tokens: hash with `auth::token::hash_token()`, look up in `api_tokens`. For passwords: verify with `auth::password::verify_password()`.
- Then verify permissions via `resolver::has_permission()`

### Crate API gotchas (from 01+02)
- **rand 0.10**: Use `rand::fill(&mut bytes)` (free function), not `rng().fill_bytes()`
- **axum 0.8**: `.patch()`, `.put()`, `.delete()` are `MethodRouter` methods — chain directly, don't import from `axum::routing`
- **Clippy**: Functions with 7+ params need a params struct. `Copy` types use `self` not `&self`.
- **sqlx**: After adding `sqlx::query!()` calls, run `just db-prepare` to update `.sqlx/` cache. Commit `.sqlx/` changes.
- **Audit logging**: Use `AuditEntry` struct pattern with `write_audit()` for mutations (see `api/users.rs` or `api/admin.rs`).
