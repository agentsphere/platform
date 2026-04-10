# Schema Review Findings

Database schema review of 48 tables across 70 migrations. 20 findings ordered by severity.
Includes feedback from Gemini review (#2, #9, #10, #20).

---

## Critical

### 1. `metric_series` unique constraint missing `project_id`

**Constraint:** `UNIQUE (name, labels)` — does NOT include `project_id`.

Two different projects submitting a metric with the same name and labels collide
into one series row. If Project A and Project B both emit `http_requests_total {}`,
they share one series. The `project_id` column exists but is nullable and excluded
from the unique constraint — whichever project writes first "owns" the series row.

**Impact:** Cross-project metric contamination. Project B's samples get attributed
to Project A's `project_id` on the shared series row.

**Fix:** Migration to replace the unique constraint:

```sql
-- Step 1: Drop old constraint
ALTER TABLE metric_series DROP CONSTRAINT metric_series_name_labels_key;

-- Step 2: Add new constraint including project_id
-- NULLS NOT DISTINCT ensures (name, labels, NULL) is also unique
ALTER TABLE metric_series
  ADD CONSTRAINT metric_series_name_labels_project_key
  UNIQUE NULLS NOT DISTINCT (name, labels, project_id);
```

**Code changes:** Update `ON CONFLICT (name, labels)` in `src/observe/store.rs`
to `ON CONFLICT (name, labels, project_id)`. Affects `write_metrics()` (batched
UNNEST query) and any other upsert paths.

**Risk:** Existing data may have duplicate `(name, labels)` rows once `project_id`
is included — need to verify no conflicts before applying. Pre-alpha so likely safe.

---

## High

### 2. Soft-delete name collision on `projects`

**Constraint:** `UNIQUE (owner_id, name)` — table-level, not partial.

Projects use soft-delete (`is_active = false`). If a user creates project
"backend", deletes it (soft), then tries to create a new "backend", the DB
rejects it with a unique constraint violation — the old soft-deleted row still
physically exists.

Compare with `namespace_slug` which already uses a partial unique index:
`UNIQUE (namespace_slug) WHERE is_active = true`.

**Fix:**

```sql
ALTER TABLE projects DROP CONSTRAINT projects_owner_id_name_key;
CREATE UNIQUE INDEX idx_projects_owner_name_active
  ON projects(owner_id, name)
  WHERE is_active = true;
```

### 3. `comments` — missing indexes on `issue_id` and `mr_id`

Listing comments for an issue does `WHERE issue_id = $1`. Listing comments for
an MR does `WHERE mr_id = $1`. Neither column is indexed. Sequential scan on
every comment listing.

**Fix:**

```sql
CREATE INDEX idx_comments_issue ON comments(issue_id) WHERE issue_id IS NOT NULL;
CREATE INDEX idx_comments_mr ON comments(mr_id) WHERE mr_id IS NOT NULL;
```

### 4. `webhooks` — no index on `project_id`

`fire_webhooks()` queries `WHERE project_id = $1 AND active = true` on every
mutation. No index exists.

**Fix:**

```sql
CREATE INDEX idx_webhooks_project ON webhooks(project_id) WHERE active = true;
```

### 5. `agent_messages` — no index on `session_id`

Listing messages for a session does `WHERE session_id = $1 ORDER BY created_at`.
No index at all. Full table scan for every message listing.

**Fix:**

```sql
CREATE INDEX idx_agent_messages_session ON agent_messages(session_id, created_at);
```

### 6. `mr_reviews` — no index on `mr_id`

Listing reviews for a merge request queries `WHERE mr_id = $1`. No index.

**Fix:**

```sql
CREATE INDEX idx_mr_reviews_mr ON mr_reviews(mr_id);
```

---

## Medium

### 7. `user_roles` — nullable column in unique constraint allows duplicates

`UNIQUE (user_id, role_id, project_id)` where `project_id` is nullable. In
PostgreSQL, `NULL != NULL` in unique constraints, so multiple rows with the same
`(user_id, role_id, NULL)` are considered distinct. A user can be granted the
same global role multiple times.

**Fix:**

```sql
-- Option A: Postgres 15+ NULLS NOT DISTINCT
ALTER TABLE user_roles DROP CONSTRAINT user_roles_user_id_role_id_project_id_key;
ALTER TABLE user_roles
  ADD CONSTRAINT user_roles_user_id_role_id_project_id_key
  UNIQUE NULLS NOT DISTINCT (user_id, role_id, project_id);

-- Option B: Partial unique index (works on all Postgres versions)
CREATE UNIQUE INDEX idx_user_roles_global
  ON user_roles(user_id, role_id)
  WHERE project_id IS NULL;
```

### 8. `delegations` — same nullable uniqueness issue

`UNIQUE (delegator_id, delegate_id, permission_id, project_id)` with nullable
`project_id`. Same problem as #7 — duplicate global delegations possible.

**Fix:** Same approach as #7 — `NULLS NOT DISTINCT` or partial unique index.

### 9. UUIDv4 B-Tree fragmentation on high-volume observability tables

Tables `log_entries`, `spans`, `traces` use `DEFAULT gen_random_uuid()` (UUIDv4
— completely random). At high insert volumes, random UUIDs cause massive B-Tree
index page splits — every insert touches a random page, ruining cache locality
and degrading insert throughput.

**Impact:** Write amplification and index bloat on tables that ingest thousands
of rows per second. The PK index becomes fragmented across the entire B-Tree.

**Fix:** No migration needed. Generate UUIDv7 (time-ordered) in Rust and insert
explicitly instead of relying on `DEFAULT gen_random_uuid()`.

```toml
# Cargo.toml — enable v7 feature
uuid = { version = "1", features = ["v4", "v7"] }
```

```rust
// In observe/store.rs — use Uuid::now_v7() for high-volume inserts
let id = uuid::Uuid::now_v7();
```

UUIDv7 is timestamp-prefixed, so inserts are always appending to the end of the
B-Tree — sequential I/O, perfect cache locality, no page splits.

**Scope:** Only worth doing for `log_entries`, `spans`, `traces`, and
`metric_samples` (high volume). Other tables with low insert rates (users,
projects, etc.) are fine with v4.

### 10. `artifacts` — missing GC index on `expires_at`

The artifact cleanup worker queries
`WHERE expires_at IS NOT NULL AND expires_at < now()` (see `main.rs:508`).
No index on `expires_at` — sequential scan of entire artifacts table every
cleanup cycle.

**Fix:**

```sql
CREATE INDEX idx_artifacts_expires_at
  ON artifacts(expires_at)
  WHERE expires_at IS NOT NULL;
```

### 11. `secrets.scope` and `secrets.environment` — overlapping semantics

`scope` CHECK: `('all', 'pipeline', 'agent', 'test', 'staging', 'prod')`
`environment` CHECK: `(NULL, 'preview', 'staging', 'production')`

Both have a "staging" concept. `scope` was the original access-control dimension,
`environment` was added later for deployment-tier filtering. The unique index
`idx_secrets_scoped` uses `environment` (not `scope`) for environment
discrimination.

**Verdict:** Historical evolution. Not urgent but confusing. Consider deprecating
the environment-like values from `scope` (`'staging'`, `'prod'`) in a future
cleanup, keeping `scope` as pure access-control (`'all'`, `'pipeline'`, `'agent'`,
`'test'`).

---

## Low

### 12. `notifications` — index could include sort column

Current index: `(user_id, status)`. Common query pattern is probably
`WHERE user_id = $1 AND status = 'pending' ORDER BY created_at DESC`.

**Fix (optional):**

```sql
DROP INDEX idx_notifications_user_status;
CREATE INDEX idx_notifications_user_status
  ON notifications(user_id, status, created_at DESC);
```

### 13. Observability FKs use RESTRICT (default) but safe due to soft-delete

`traces.project_id`, `spans.project_id`, `log_entries.project_id` all use bare
`REFERENCES projects(id)` (default RESTRICT). A hard `DELETE FROM projects` would
fail if any observability data references that project.

**Verdict:** Safe — projects use soft-delete (`is_active = false`), never hard
delete. No action needed unless hard-delete is ever introduced.

### 14. No time-based partitioning on high-volume tables

`log_entries`, `spans`, `metric_samples` grow unbounded. Retention cleanup uses
batched DELETE (production-hardening.md Section 5). Time-based partitioning would
make cleanup instant via `DROP PARTITION`.

**Verdict:** Future optimization. Batched DELETE is adequate at current scale.
Consider partitioning when tables exceed ~100M rows.

### 15. `registry_blob_links.blob_digest` is TEXT, not FK

The composite PK is `(repository_id, blob_digest)` but `blob_digest` is just
TEXT with no FK to `registry_blobs.digest`. Orphaned blob links are possible if
blobs are deleted directly.

**Verdict:** Standard for OCI registries. Blob GC is a separate mark-and-sweep
concern. No fix needed.

---

## Informational

### 16. `audit_log.actor_id` — no FK to `users(id)`

Intentional. Audit records must survive user deletion for compliance. The
`actor_name` TEXT column preserves the display name independently.

### 17. `deploy_releases.phase` — 10-value CHECK constraint

Adding a new phase requires a migration to alter the CHECK. This is by design —
the DB enforces valid state machine states. Matches the Rust enum pattern used
throughout.

### 18. Missing `updated_at` on some mutable tables

- `webhooks` — can be toggled active/inactive
- `alert_rules` — can be enabled/disabled
- `api_tokens` — has `last_used_at` but no general `updated_at`

Minor. These tables have limited mutation patterns.

### 19. `pipeline_steps` has `started_at` but no `finished_at`

`duration_ms` exists, so finish time can be reconstructed as
`started_at + duration_ms`. Not worth adding a column.

### 20. `metric_samples` — microsecond timestamp collision silently drops data

PK is `(series_id, timestamp)` with `TIMESTAMPTZ` (microsecond precision).
The batch insert uses `ON CONFLICT DO NOTHING`. If two samples for the same
series have the exact same microsecond timestamp, the second is silently dropped.

**Verdict:** Acceptable for DevOps metrics — two samples at the same microsecond
are effectively duplicates. Would matter for strict billing/cost accounting, but
`metric_samples` is purely observability data. No fix needed.

---

## Consolidated Schema Reference

### Table Count by Domain

| Domain | Tables | Count |
|--------|--------|-------|
| Auth & Users | users, auth_sessions, api_tokens, passkey_credentials, user_ssh_keys, user_gpg_keys, user_provider_keys, cli_credentials, setup_tokens, llm_provider_configs | 10 |
| RBAC | roles, permissions, role_permissions, user_roles, delegations | 5 |
| Workspaces | workspaces, workspace_members | 2 |
| Projects & PM | projects, issues, merge_requests, mr_reviews, comments, webhooks, branch_protection_rules, releases, release_assets | 9 |
| Pipelines & CI | pipelines, pipeline_steps, artifacts | 3 |
| Deployments | deploy_targets, deploy_releases, rollout_analyses, release_history, ops_repos | 5 |
| Feature Flags | feature_flags, feature_flag_rules, feature_flag_overrides, feature_flag_history | 4 |
| Agent Sessions | agent_sessions, agent_messages | 2 |
| OCI Registry | registry_repositories, registry_blobs, registry_blob_links, registry_manifests, registry_tags | 5 |
| Observability | traces, spans, log_entries, metric_series, metric_samples | 5 |
| Alerting | alert_rules, alert_events | 2 |
| Secrets | secrets | 1 |
| Notifications | notifications | 1 |
| Audit | audit_log | 1 |
| Platform | platform_commands, platform_settings | 2 |
| Mesh | mesh_ca, mesh_certs | 2 |
| **Total** | | **59** |

### Index Summary

48 explicit indexes across all tables (excludes PK/UNIQUE auto-indexes).

### Functions & Triggers

- `set_updated_at()` trigger function — used on 11 tables
- No other stored procedures or functions
