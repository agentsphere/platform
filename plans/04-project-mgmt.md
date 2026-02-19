# 04 — Project Management

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
- `DELETE /api/projects/:id` — soft-delete project (mark inactive)
  - Requires: admin or owner

### 2. `src/api/issues.rs` — Issues & Comments

Issues use project-scoped auto-incrementing numbers (not UUID in the URL).

- `POST /api/projects/:id/issues` — create issue
  - Required: `title`; Optional: `body`, `labels`, `assignee_id`
  - Auto-assign `number` = max(number) + 1 for this project
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

### 3. `src/api/merge_requests.rs` — Merge Requests & Reviews

- `POST /api/projects/:id/merge-requests` — create MR
  - Required: `source_branch`, `target_branch`, `title`
  - Optional: `body`
  - Auto-assign `number`
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
