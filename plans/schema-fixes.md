# Schema Fixes — Critical & High Findings

Addresses findings #1-#6 from `plans/schema-review-findings.md`:
1 critical (cross-project metric collision) + 5 high (soft-delete name trap,
4 missing indexes).

**No new crate dependencies.** All changes use existing deps + SQL migrations.

**Estimated effort:** ~2 hours total.

---

## PR 1: Fix `metric_series` unique constraint (Critical #1)

### Problem

`UNIQUE (name, labels)` on `metric_series` does not include `project_id`.
Two projects emitting `http_requests_total {}` collide into one series row.
The `ON CONFLICT (name, labels)` upsert in `store.rs` overwrites whichever
project wrote first.

### Migration

**File:** `migrations/20260410020001_metric_series_project_unique.up.sql`

```sql
-- Drop the old constraint that ignores project_id.
ALTER TABLE metric_series DROP CONSTRAINT metric_series_name_labels_key;

-- Add new constraint including project_id.
-- NULLS NOT DISTINCT: (name, labels, NULL) is also unique — prevents
-- two "unscoped" series with the same name from colliding silently.
ALTER TABLE metric_series
  ADD CONSTRAINT metric_series_name_labels_project_key
  UNIQUE NULLS NOT DISTINCT (name, labels, project_id);
```

**File:** `migrations/20260410020001_metric_series_project_unique.down.sql`

```sql
ALTER TABLE metric_series DROP CONSTRAINT metric_series_name_labels_project_key;
ALTER TABLE metric_series ADD CONSTRAINT metric_series_name_labels_key UNIQUE (name, labels);
```

### Code changes

**File:** `src/observe/store.rs` — `write_metrics()` (~line 275)

```rust
// Before
ON CONFLICT (name, labels)
DO UPDATE SET last_value = EXCLUDED.last_value, updated_at = now()

// After — match the new unique constraint
ON CONFLICT (name, labels, project_id)
DO UPDATE SET
    last_value = EXCLUDED.last_value,
    metric_type = EXCLUDED.metric_type,
    unit = EXCLUDED.unit,
    updated_at = now()
```

Also update the comment above the query:

```rust
// Before
// ON CONFLICT matches the UNIQUE (name, labels) constraint.

// After
// ON CONFLICT matches the UNIQUE (name, labels, project_id) constraint.
```

### sqlx cache

The query is dynamic (`sqlx::query_as(...)` not `sqlx::query_as!`), so no
`.sqlx/` cache update needed.

### Test plan

**Unit** (in `src/observe/store.rs` — none needed, no unit-testable logic changed)

**Integration** — add to `tests/observe_integration.rs`:

```rust
/// Two projects writing the same metric name get separate series.
#[sqlx::test(migrations = "./migrations")]
async fn metrics_different_projects_get_separate_series(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let proj_a = helpers::create_project(&app, &admin_token, "metrics-a", "private").await;
    let proj_b = helpers::create_project(&app, &admin_token, "metrics-b", "private").await;

    // Ingest same metric name for both projects via OTLP endpoint
    // (or call write_metrics directly if simpler)
    let now = chrono::Utc::now();
    let metrics = vec![
        crate::observe::store::MetricRecord {
            name: "http_requests_total".into(),
            labels: serde_json::json!({}),
            metric_type: "counter".into(),
            unit: None,
            project_id: Some(proj_a),
            value: 100.0,
            timestamp: now,
        },
        crate::observe::store::MetricRecord {
            name: "http_requests_total".into(),
            labels: serde_json::json!({}),
            metric_type: "counter".into(),
            unit: None,
            project_id: Some(proj_b),
            value: 200.0,
            timestamp: now,
        },
    ];
    crate::observe::store::write_metrics(&state.pool, &metrics)
        .await
        .expect("write_metrics should succeed");

    // Verify two distinct series exist
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM metric_series WHERE name = 'http_requests_total'",
    )
    .bind()
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(count.0, 2, "each project should have its own series");

    // Verify project_ids are correct
    let series: Vec<(Option<uuid::Uuid>, Option<f64>)> = sqlx::query_as(
        "SELECT project_id, last_value FROM metric_series WHERE name = 'http_requests_total' ORDER BY last_value",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(series[0].0, Some(proj_a));
    assert_eq!(series[0].1, Some(100.0));
    assert_eq!(series[1].0, Some(proj_b));
    assert_eq!(series[1].1, Some(200.0));
}
```

**Existing tests:** Verify all observe/metric integration tests still pass —
the constraint change is backward-compatible (existing data with unique
`(name, labels)` pairs will also be unique under `(name, labels, project_id)`).

> **Status: COMPLETE** — Migration `20260410020001` applied. `ON CONFLICT` updated
> in store.rs. Also added `metrics_same_project_upserts_series` test for upsert path.

---

## PR 2: Fix soft-delete name collision on `projects` (High #2)

### Problem

`UNIQUE (owner_id, name)` on `projects` is a table-level constraint that
includes soft-deleted rows. A user who deletes project "backend" cannot create
a new project named "backend" — the DB rejects it.

The `namespace_slug` column already uses the correct pattern:
`CREATE UNIQUE INDEX ... WHERE is_active = true`.

### Migration

**File:** `migrations/20260410020002_project_name_partial_unique.up.sql`

```sql
-- Drop table-level constraint (includes soft-deleted rows).
ALTER TABLE projects DROP CONSTRAINT projects_owner_id_name_key;

-- Replace with partial unique index (active rows only).
-- Matches the pattern used for namespace_slug.
CREATE UNIQUE INDEX idx_projects_owner_name_active
  ON projects(owner_id, name)
  WHERE is_active = true;
```

**File:** `migrations/20260410020002_project_name_partial_unique.down.sql`

```sql
DROP INDEX IF EXISTS idx_projects_owner_name_active;
ALTER TABLE projects ADD CONSTRAINT projects_owner_id_name_key UNIQUE (owner_id, name);
```

### Code changes

**File:** `src/api/projects.rs` — `try_insert_project()` (~line 185-187)

The error handler checks constraint names with `.contains("owner_id") || .contains("name")`.
The new index name `idx_projects_owner_name_active` contains both substrings,
so **no code change needed** — the existing error handling works as-is.

Verify this explicitly:
```rust
// Line 185-187 — existing code, no change needed:
} else if db_err
    .constraint()
    .is_some_and(|c| c.contains("owner_id") || c.contains("name"))
{
    // "idx_projects_owner_name_active" contains both "owner" and "name" ✓
```

### sqlx cache

The INSERT query is `sqlx::query_as!` (compile-time checked) but the query text
itself doesn't change — only the constraint definition changes. No `.sqlx/`
update needed.

### Test plan

**Integration** — add to `tests/project_integration.rs`:

```rust
/// After soft-deleting a project, recreating with the same name succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn recreate_project_after_soft_delete(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create project
    let id1 = helpers::create_project(&app, &admin_token, "recycle-name", "private").await;

    // Soft-delete
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/projects/{id1}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Recreate with same name — should succeed
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "recycle-name" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "recreate after delete should work: {body}");

    let id2 = body["id"].as_str().unwrap();
    assert_ne!(id1.to_string(), id2, "new project should have a different id");
}

/// Two active projects with the same name still conflict.
#[sqlx::test(migrations = "./migrations")]
async fn duplicate_active_project_name_still_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::create_project(&app, &admin_token, "unique-proj", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "unique-proj" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("already exists"));
}
```

**Existing tests:** The existing `create_project_duplicate_name` test verifies
active-active collision still works.

> **Status: COMPLETE** — Migration `20260410020002` applied.
> `recreate_project_after_soft_delete` test added. No code changes needed —
> error handler constraint name check works with new index name.

---

## PR 3: Add missing indexes (High #3-#6)

### Problem

Four tables lack indexes on columns used in frequent queries:
- `comments` — no index on `issue_id` or `mr_id`
- `webhooks` — no index on `project_id`
- `agent_messages` — no index on `session_id`
- `mr_reviews` — no index on `mr_id`

### Migration

All indexes go in one migration — they're all independent, non-breaking additions.

**File:** `migrations/20260410020003_missing_indexes.up.sql`

```sql
-- comments: listing comments for an issue
-- Query: SELECT ... FROM comments WHERE issue_id = $1 ORDER BY created_at ASC
CREATE INDEX idx_comments_issue ON comments(issue_id, created_at ASC)
  WHERE issue_id IS NOT NULL;

-- comments: listing comments for an MR
-- Query: SELECT ... FROM comments WHERE mr_id = $1 ORDER BY created_at ASC
CREATE INDEX idx_comments_mr ON comments(mr_id, created_at ASC)
  WHERE mr_id IS NOT NULL;

-- webhooks: listing + fire_webhooks dispatch
-- Query: SELECT ... FROM webhooks WHERE project_id = $1 AND active = true
CREATE INDEX idx_webhooks_project ON webhooks(project_id)
  WHERE active = true;

-- agent_messages: listing messages + progress fetch + idle check
-- Query: SELECT ... FROM agent_messages WHERE session_id = $1 ORDER BY created_at
CREATE INDEX idx_agent_messages_session ON agent_messages(session_id, created_at);

-- mr_reviews: listing reviews + approval count for merge validation
-- Query: SELECT ... FROM mr_reviews WHERE mr_id = $1 ORDER BY created_at ASC
CREATE INDEX idx_mr_reviews_mr ON mr_reviews(mr_id, created_at ASC);
```

**File:** `migrations/20260410020003_missing_indexes.down.sql`

```sql
DROP INDEX IF EXISTS idx_comments_issue;
DROP INDEX IF EXISTS idx_comments_mr;
DROP INDEX IF EXISTS idx_webhooks_project;
DROP INDEX IF EXISTS idx_agent_messages_session;
DROP INDEX IF EXISTS idx_mr_reviews_mr;
```

### Index design rationale

| Index | Covers query | Why this shape |
|-------|-------------|----------------|
| `comments(issue_id, created_at)` | `WHERE issue_id = $1 ORDER BY created_at` | Composite covers both filter + sort; partial excludes MR comments |
| `comments(mr_id, created_at)` | `WHERE mr_id = $1 ORDER BY created_at` | Same — partial excludes issue comments |
| `webhooks(project_id) WHERE active` | `WHERE project_id = $1 AND active = true` | Partial index: only active webhooks matter for dispatch |
| `agent_messages(session_id, created_at)` | `WHERE session_id = $1 ORDER BY created_at` | Composite covers list + progress + idle timeout queries |
| `mr_reviews(mr_id, created_at)` | `WHERE mr_id = $1 ORDER BY created_at` | Composite covers review list + approval count |

**Note:** These are regular indexes (not `CONCURRENTLY`) because the tables are
small in pre-alpha. If this runs on a production DB with large tables, convert
to `-- no-transaction` + `CONCURRENTLY` per index.

### Code changes

None. All queries already use the correct WHERE/ORDER BY patterns — adding
indexes is transparent.

### sqlx cache

No query text changes — no `.sqlx/` update needed.

### Test plan

No new tests needed — indexes don't change query semantics. Existing integration
tests verify the queries still work. The indexes only affect performance.

**Verify:** Run `just test-integration` to confirm no regressions.

> **Status: COMPLETE** — Migration `20260410020003` applied. 5 indexes created.

---

## Implementation Order

```
PR 1: metric_series constraint     (30 min) — critical, code + migration
PR 2: projects partial unique      (20 min) — high, migration + test
PR 3: missing indexes              (10 min) — high, migration only
```

All three PRs are independent and can be implemented in any order. They touch
no overlapping files:

| PR | Migration | Code files | Test files |
|----|-----------|------------|------------|
| 1 | `020001_metric_series_project_unique` | `src/observe/store.rs` | `tests/observe_integration.rs` |
| 2 | `020002_project_name_partial_unique` | (none) | `tests/project_integration.rs` |
| 3 | `020003_missing_indexes` | (none) | (none) |

---

## Verification

After all three PRs:

```bash
just db-migrate       # apply all 3 migrations
just db-prepare       # regenerate .sqlx/ (PR 1 changed a dynamic query, but just in case)
just test-unit        # fast sanity check
just test-integration # full API/DB verification
```
