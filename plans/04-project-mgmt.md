# 04 — Project Management ✅ Complete (2026-02-19)

## Prerequisite
- 01-foundation complete (store, AppState)
- 02-identity-auth complete (AuthUser, RequirePermission)

## Blocks
- Nothing directly (other modules reference `projects` table but via DB, not code dependency)

## Can Parallelize With
- 03-git-server, 05-build-engine, 06-deployer, 07-agent, 08-observability, 09-secrets-notify

---

## Scope

Project CRUD, issues with comments, merge requests with reviews, webhooks. The "project management lite" layer — enough for agents and humans to track work, not a full Jira clone.

---

## Deliverables

### 1. `src/api/projects.rs` — Project CRUD

- `POST /api/projects` — create project
  - Required: `name` (slug), `visibility`
  - Optional: `display_name`, `description`, `default_branch`
  - Auto-set `owner_id` from authenticated user
  - Initialize bare git repo (calls `git::init_bare_repo`)
  - Requires: `project:write` (global) or admin role
- `GET /api/projects` — list projects
  - Filter by: `owner_id`, `visibility`, search by name
  - Paginated (cursor-based using `created_at`)
  - Public/internal projects visible to all authenticated users; private only to members
- `GET /api/projects/:id` — get project detail
  - Requires: `project:read` on this project (or public visibility)
- `PATCH /api/projects/:id` — update project settings
  - Requires: `project:write` on this project or owner
- `DELETE /api/projects/:id` — soft-delete project (set `is_active = false`)
  - Requires: admin or owner
  - Soft-deleted projects are excluded from list queries by default

### 2. `src/api/issues.rs` — Issues & Comments

Issues use project-scoped auto-incrementing numbers (not UUID in the URL).

- `POST /api/projects/:id/issues` — create issue
  - Required: `title`; Optional: `body`, `labels`, `assignee_id`
  - Auto-assign `number` via atomic increment on `projects.next_issue_number`:
    ```sql
    UPDATE projects SET next_issue_number = next_issue_number + 1
    WHERE id = $1 RETURNING next_issue_number
    ```
    This avoids race conditions vs `max(number) + 1`.
  - Requires: `project:write`
- `GET /api/projects/:id/issues` — list issues
  - Filter by: `status` (open/closed), `labels`, `assignee_id`
  - Paginated
  - Requires: `project:read`
- `GET /api/projects/:id/issues/:number` — get issue
  - Includes comments (embedded or separate endpoint)
  - Requires: `project:read`
- `PATCH /api/projects/:id/issues/:number` — update issue
  - Can update: title, body, status, labels, assignee
  - Requires: `project:write` or issue author
- `POST /api/projects/:id/issues/:number/comments` — add comment
  - Requires: `project:write` (or `project:read` if allowing read-only users to comment — decide at implementation)
- `PATCH /api/projects/:id/issues/:number/comments/:comment_id` — edit comment
  - Requires: comment author or admin

**Note:** The `comments` table now supports both issues and MRs. The same CRUD endpoints apply to MRs:
- `POST /api/projects/:id/merge-requests/:number/comments` — add comment to MR
- `PATCH /api/projects/:id/merge-requests/:number/comments/:comment_id` — edit MR comment

Comments set either `issue_id` or `mr_id` (never both). The `project_id` is denormalized on the row.

### 3. `src/api/merge_requests.rs` — Merge Requests & Reviews

- `POST /api/projects/:id/merge-requests` — create MR
  - Required: `source_branch`, `target_branch`, `title`
  - Optional: `body`
  - Auto-assign `number` via atomic increment on `projects.next_mr_number` (same pattern as issues)
  - Validate: source_branch exists in repo, source != target
  - Requires: `project:write`
- `GET /api/projects/:id/merge-requests` — list MRs
  - Filter by: `status` (open/merged/closed), `author_id`
  - Requires: `project:read`
- `GET /api/projects/:id/merge-requests/:number` — get MR detail
  - Include: reviews, diff stats
  - Requires: `project:read`
- `PATCH /api/projects/:id/merge-requests/:number` — update MR
  - Can update: title, body, status (close only — merge is separate)
  - Requires: `project:write` or MR author
- `POST /api/projects/:id/merge-requests/:number/merge` — merge MR
  - Execute: `git merge --no-ff source_branch` on target_branch in bare repo
  - Update MR status to `merged`, set `merged_by`, `merged_at`
  - Requires: `project:write`
- `POST /api/projects/:id/merge-requests/:number/reviews` — submit review
  - Verdict: `approve`, `request_changes`, `comment`
  - Requires: `project:read` (anyone can review)

### 4. `src/api/webhooks.rs` — Webhook Management

Webhook CRUD for projects:

- `POST /api/projects/:id/webhooks` — create webhook
  - Required: `url`, `events` (array of: `push`, `mr`, `issue`, `build`, `deploy`)
  - Optional: `secret` (HMAC key for payload signing)
  - Requires: `project:write`
- `GET /api/projects/:id/webhooks` — list webhooks
- `PATCH /api/projects/:id/webhooks/:wh_id` — update webhook
- `DELETE /api/projects/:id/webhooks/:wh_id` — delete webhook
- `POST /api/projects/:id/webhooks/:wh_id/test` — send test payload

Webhook dispatch (shared utility, used by other modules too):
- `pub async fn fire_webhooks(pool: &PgPool, project_id: Uuid, event: &str, payload: &serde_json::Value) -> Result<()>`
  - Query active webhooks for this project + event
  - For each: spawn tokio task to POST payload to URL
  - If `secret` set: compute HMAC-SHA256 of body, include in `X-Platform-Signature` header
  - Log delivery status

### 5. `src/api/mod.rs` — Router Assembly

Build the main API router merging all route groups:
```rust
pub fn router(state: AppState) -> Router {
    Router::new()
        .nest("/api", api_routes(state.clone()))
        .merge(git::routes(state.clone()))  // git smart HTTP at root level
        .with_state(state)
}

fn api_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .merge(users::routes())
        .merge(projects::routes())
        .merge(issues::routes())
        .merge(merge_requests::routes())
        // ... other modules add their routes here
}
```

### 6. `src/api/health.rs` — Health Check

Move existing `/healthz` handler here:
- `GET /healthz` — returns 200 + JSON `{"status": "ok", "version": env!("CARGO_PKG_VERSION")}`
- Optional: check DB connectivity, report degraded if Valkey is down

---

## Data Flow

```
Create project → init bare repo → project row in DB
Create issue   → issue row + auto-number → fire webhook (issue.created)
Create MR      → MR row + auto-number → fire webhook (mr.created)
Merge MR       → git merge in bare repo → update MR status → fire webhook (mr.merged)
                                        → trigger pipeline (if .platform.yaml exists)
```

---

## Testing

- Integration:
  - Create project → list → get → update → verify
  - Create issue → add comment → close issue → verify status
  - Create MR → submit review → merge → verify git state + MR status
  - Webhook: create → fire event → verify HTTP POST received (use mock server or log)
  - Permission checks: non-member can't write to private project
  - Auto-incrementing issue/MR numbers are project-scoped and correct

## Done When

1. Full project CRUD with visibility rules
2. Issues with comments and status transitions
3. MRs with review and merge (actual git merge in bare repo)
4. Webhooks fire on events with HMAC signing
5. All endpoints enforce RBAC
6. Router assembles all API routes

## Estimated LOC
~1,000 Rust

---

## Foundation & Auth Context (from 01+02 implementation)

Things the implementor must know from completed phases:

### What already exists
- **`src/store::AppState`** — `{ pool: PgPool, valkey: fred::clients::Pool, minio: opendal::Operator, kube: kube::Client, config: Arc<Config> }`
- **`src/auth/middleware::AuthUser`** — axum `FromRequestParts` extractor. Fields: `user_id: Uuid`, `user_name: String`, `ip_addr: Option<String>`. Checks Bearer token then session cookie.
- **`src/rbac::Permission`** — enum with 13 variants. `as_str(self)` takes `self` by value (it's `Copy`).
- **`src/rbac::resolver`** — `has_permission(pool, valkey, user_id, project_id, perm) -> Result<bool>`
- **`src/rbac::middleware::require_permission`** — route-layer middleware. Usage:
  ```rust
  .route_layer(axum::middleware::from_fn_with_state(
      state.clone(),
      require_permission(Permission::ProjectRead),
  ))
  ```
  Extracts `project_id` from `/projects/:id` path segments automatically.
- **`src/error::ApiError`** — `NotFound`, `Unauthorized`, `Forbidden`, `BadRequest`, `Conflict`, `Internal`. Has `From<sqlx::Error>` (maps 23505 unique violation → `Conflict`).
- **`src/api/mod.rs`** — `pub fn router() -> Router<AppState>`. Currently merges `users::router()` and `admin::router()`. Add new module routers here.

### Router pattern
The existing `api::router()` in `src/api/mod.rs` merges sub-routers:
```rust
pub fn router() -> Router<AppState> {
    Router::new().merge(users::router()).merge(admin::router())
}
```
Add `projects::router()`, `issues::router()`, etc. following the same pattern.

### Permission checking patterns
Two approaches exist in the codebase:
1. **Inline** (used by admin.rs): call `resolver::has_permission()` directly in the handler. Simpler for routes needing different permissions.
2. **Route-layer** (available via `require_permission`): apply to a group of routes sharing one permission. Better for project-scoped endpoints.

The `require_permission` middleware auto-extracts `project_id` from `/projects/:id` in the URL path, so project-scoped permissions work automatically.

### Notes on this module's plan
- **Section 5 (`src/api/mod.rs`)**: The router assembly described in the plan is more elaborate than what currently exists. The actual pattern is simpler — just `.merge()` calls. Don't restructure the existing router; just add new sub-module routers.
- **Section 6 (`src/api/health.rs`)**: The `/healthz` endpoint already exists inline in `main.rs`. Moving it to a separate file is optional.

### Crate API gotchas (from 01+02)
- **axum 0.8**: `.patch()`, `.put()`, `.delete()` are `MethodRouter` methods — chain directly (e.g., `.route("/path", get(list).post(create))`), don't import from `axum::routing`
- **Clippy**: Functions with 7+ params need a params struct. `Copy` types use `self` not `&self`.
- **sqlx**: After adding `sqlx::query!()` calls, run `just db-prepare` to update `.sqlx/` cache. Commit `.sqlx/` changes.
- **Audit logging**: Use `AuditEntry` struct pattern with `write_audit()` for mutations (see `api/users.rs` or `api/admin.rs`).
- **Pagination**: `ListParams { limit: Option<i64>, offset: Option<i64> }` and `ListResponse<T> { items: Vec<T>, total: i64 }` already used in `api/users.rs` — consider sharing or defining per-module.

---

## Implementation Notes (2026-02-19)

### Deviations from plan

1. **`require_permission` route layer not used** — Sub-routers return `Router<AppState>` without a concrete state, so `from_fn_with_state` can't be called at construction time. All permission checks are inline in handlers instead. Webhooks use a `require_project_write()` helper function pattern.

2. **`src/api/health.rs` not created** — `/healthz` stays inline in `main.rs` as-is. No value in extracting a one-liner.

3. **`ListParams`/`ListResponse` defined per-module** — Each API file defines its own (slightly different) `ListParams` struct with module-specific filter fields. Shared generic not worth the coupling.

4. **No newtype IDs yet** — Raw `Uuid` used throughout, matching Phase 02 conventions. Can add newtypes in a later pass.

5. **MR merge uses `git worktree`** — Bare repos can't merge directly. The implementation creates a temporary worktree, merges, and cleans up. Uses `origin/{source_branch}` ref syntax.

6. **`hmac` crate added** — Direct dependency for webhook HMAC-SHA256 signing (was transitive before).

### Files created
- `src/api/projects.rs` — Project CRUD (~250 LOC)
- `src/api/issues.rs` — Issues + comments (~370 LOC)
- `src/api/merge_requests.rs` — MRs + reviews + comments + merge (~580 LOC)
- `src/api/webhooks.rs` — Webhook CRUD + fire_webhooks (~400 LOC)

### Files modified
- `src/api/mod.rs` — Added 4 new module declarations + `.merge()` calls
- `Cargo.toml` — Added `hmac = "0.12"`
