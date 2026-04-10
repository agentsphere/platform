# Schema Fixes — Remaining Findings (#7–#20)

Addresses findings #7–#20 from `plans/schema-review-findings.md`. Categorized
by actionability.

---

## Actionable (implement now)

### A1. Nullable uniqueness on `user_roles` and `delegations` (Medium #7, #8)

**Problem:** Both tables have `UNIQUE (..., project_id)` where `project_id` is
nullable. In PostgreSQL, `UNIQUE` treats NULLs as distinct — a user can be
granted the same global role (project_id = NULL) multiple times. Same for
delegations.

Current constraint names (auto-generated):
- `user_roles_user_id_role_id_project_id_key`
- `delegations_delegator_id_delegate_id_permission_id_project_id_key`

**INSERT paths that could create duplicates:**
- `src/api/admin.rs:501` — assign role (no ON CONFLICT)
- `src/api/setup.rs:128` — first-time setup (no ON CONFLICT)
- `src/agent/identity.rs:78` — agent identity (no ON CONFLICT)
- `src/rbac/delegation.rs:106` — create delegation (no ON CONFLICT)

The `src/store/bootstrap.rs:480` and `src/deployer/reconciler.rs:1277` paths
already use `ON CONFLICT DO NOTHING`, so they're safe.

#### Migration

**File:** `migrations/20260410030001_nullable_unique_fixes.up.sql`

```sql
-- user_roles: fix nullable project_id in unique constraint.
-- NULLS NOT DISTINCT: (user_id, role_id, NULL) is now unique.
ALTER TABLE user_roles
  DROP CONSTRAINT user_roles_user_id_role_id_project_id_key;
ALTER TABLE user_roles
  ADD CONSTRAINT user_roles_user_id_role_id_project_id_key
  UNIQUE NULLS NOT DISTINCT (user_id, role_id, project_id);

-- delegations: fix nullable project_id in unique constraint.
ALTER TABLE delegations
  DROP CONSTRAINT delegations_delegator_id_delegate_id_permission_id_project_id_key;
ALTER TABLE delegations
  ADD CONSTRAINT delegations_delegator_id_delegate_id_permission_id_project_id_key
  UNIQUE NULLS NOT DISTINCT (delegator_id, delegate_id, permission_id, project_id);
```

**File:** `migrations/20260410030001_nullable_unique_fixes.down.sql`

```sql
ALTER TABLE user_roles
  DROP CONSTRAINT user_roles_user_id_role_id_project_id_key;
ALTER TABLE user_roles
  ADD CONSTRAINT user_roles_user_id_role_id_project_id_key
  UNIQUE (user_id, role_id, project_id);

ALTER TABLE delegations
  DROP CONSTRAINT delegations_delegator_id_delegate_id_permission_id_project_id_key;
ALTER TABLE delegations
  ADD CONSTRAINT delegations_delegator_id_delegate_id_permission_id_project_id_key
  UNIQUE (delegator_id, delegate_id, permission_id, project_id);
```

#### Code changes

None. The INSERT paths without ON CONFLICT will now correctly reject duplicates
(returning a 23505 error) instead of silently creating them. The callers
already return appropriate errors for constraint violations.

#### Test plan

**Integration** — add to `tests/admin_integration.rs`:

```rust
/// Assigning the same global role twice returns a conflict error.
#[sqlx::test(migrations = "./migrations")]
async fn assign_same_global_role_twice_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "dupe-role", "dupe@test.com").await;

    // First assignment — should succeed
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Second assignment of same global role — should fail
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/admin/users/{user_id}/roles"),
        serde_json::json!({ "role": "viewer" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}
```

---

### A2. UUIDv7 for high-volume observability tables (Medium #9)

**Problem:** `traces`, `spans`, `log_entries` all rely on
`DEFAULT gen_random_uuid()` (UUIDv4 — random). At high insert volumes, random
UUIDs cause B-Tree index page fragmentation. UUIDv7 is time-ordered, so inserts
always append to the end of the B-Tree.

Current state:
- `Cargo.toml`: `uuid = { version = "1", features = ["v4", "serde"] }` — no v7
- `store.rs`: all INSERT queries omit the `id` column, relying on DB DEFAULT
- `metric_samples` uses composite PK `(series_id, timestamp)` — no uuid PK

#### Cargo.toml change

```toml
# Before
uuid = { version = "1", features = ["v4", "serde"] }

# After
uuid = { version = "1", features = ["v4", "v7", "serde"] }
```

#### Code changes

**File:** `src/observe/store.rs`

For each batch insert (spans, log_entries), add a `Uuid::now_v7()` id column
to the UNNEST arrays. For traces (single row insert), add `$N` for the id.

**Spans** (~line 105): Add `id` to INSERT column list and UNNEST arrays.

```rust
// Collect ids
let mut ids: Vec<Uuid> = Vec::with_capacity(spans.len());
for _ in 0..spans.len() {
    ids.push(Uuid::now_v7());
}

// Add to query
INSERT INTO spans (id, trace_id, span_id, ...)
SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::text[], ...)
// Bind ids as first parameter, shift all others +1
.bind(&ids)
```

**Log entries** (~line 211): Same pattern — add `id` column with UUIDv7 array.

```rust
let mut ids: Vec<Uuid> = Vec::with_capacity(logs.len());
for _ in 0..logs.len() {
    ids.push(Uuid::now_v7());
}

INSERT INTO log_entries (id, timestamp, trace_id, ...)
SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[], $3::text[], ...)
.bind(&ids)
```

**Traces** (~line 145): Add `id` parameter.

```rust
let id = Uuid::now_v7();
INSERT INTO traces (id, trace_id, root_span, ...)
VALUES ($1, $2, $3, ...)
ON CONFLICT (trace_id) DO NOTHING
.bind(id)
```

#### Migration

None needed — the DB DEFAULT still exists as a fallback. We're just overriding
it from the application layer. If any other code path creates rows without
specifying id, they get v4 (harmless).

#### Test plan

**Unit** — none (Uuid::now_v7() is a library call, no custom logic)

**Integration** — existing observe tests pass (ids are transparent to query
responses). Optionally verify ids are v7-ordered:

```rust
/// Verify inserted span ids are UUIDv7 (time-ordered).
#[sqlx::test(migrations = "./migrations")]
async fn observe_spans_use_uuidv7(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    // ... ingest spans via OTLP ...
    // SELECT id FROM spans ORDER BY id — should match ORDER BY started_at
}
```

---

### A3. Artifact GC index on `expires_at` (Medium #10)

**Problem:** `cleanup_expired_artifacts()` in `main.rs:508` queries
`WHERE expires_at IS NOT NULL AND expires_at < now() AND parent_id IS NULL`.
No index on `expires_at` — sequential scan every cleanup cycle.

#### Migration

**File:** `migrations/20260410030002_artifact_gc_index.up.sql`

```sql
-- Artifact GC worker: SELECT ... WHERE expires_at IS NOT NULL AND expires_at < now()
CREATE INDEX idx_artifacts_expires_at
  ON artifacts(expires_at)
  WHERE expires_at IS NOT NULL;
```

**File:** `migrations/20260410030002_artifact_gc_index.down.sql`

```sql
DROP INDEX IF EXISTS idx_artifacts_expires_at;
```

#### Code changes

None.

#### Test plan

None — index-only change.

---

### A4. Notifications index with sort coverage (Low #12)

**Problem:** Current index `(user_id, status)` doesn't cover the
`ORDER BY created_at DESC` used in all notification queries. DB must do an
index scan + sort instead of an index-only scan.

Query patterns (from `src/api/notifications.rs`):
- `WHERE user_id = $1 AND status = $2 ORDER BY created_at DESC LIMIT $3`
- `WHERE user_id = $1 AND status IN ('pending', 'sent')` (unread count)

#### Migration

**File:** `migrations/20260410030003_notification_index_sort.up.sql`

```sql
-- Replace (user_id, status) with (user_id, status, created_at DESC)
-- to cover both the filter and the ORDER BY in notification queries.
DROP INDEX idx_notifications_user_status;
CREATE INDEX idx_notifications_user_status
  ON notifications(user_id, status, created_at DESC);
```

**File:** `migrations/20260410030003_notification_index_sort.down.sql`

```sql
DROP INDEX idx_notifications_user_status;
CREATE INDEX idx_notifications_user_status ON notifications(user_id, status);
```

#### Code changes

None.

#### Test plan

None — index-only change.

---

## Not actionable (no change needed)

### N1. `secrets.scope` / `secrets.environment` overlap (#11)

**Status:** Non-issue after investigation. The scope CHECK was updated in
migration `20260324010001_pipeline_step_types` to
`('all', 'pipeline', 'agent', 'test', 'staging', 'prod')`. The Rust validation
in `src/api/secrets.rs` matches. The overlap between scope values (`staging`,
`prod`) and environment values (`staging`, `production`) is intentional —
`scope` controls which executor can read the secret, `environment` controls
which deployment tier it belongs to. They're independent dimensions.

### N2. Observability FKs use RESTRICT (#13)

Safe — projects use soft-delete, never hard delete. No action needed.

### N3. No time-based partitioning (#14)

Future optimization. Batched DELETE is adequate at current scale. Revisit when
tables exceed ~100M rows. Would require significant migration effort (recreate
tables as partitioned, backfill data).

### N4. `registry_blob_links.blob_digest` not FK (#15)

Standard OCI registry pattern. Blob GC is separate mark-and-sweep concern.

### N5. `audit_log.actor_id` no FK (#16)

Intentional — audit records outlive users.

### N6. `deploy_releases.phase` CHECK (#17)

By design — DB enforces valid state machine states.

### N7. Missing `updated_at` on some tables (#18)

`webhooks`, `alert_rules`, `api_tokens` have limited mutation patterns.
Not worth adding columns + triggers for minimal benefit.

### N8. `pipeline_steps` no `finished_at` (#19)

`duration_ms` is sufficient. `finished_at = started_at + duration_ms`.

### N9. `metric_samples` microsecond collision (#20)

Acceptable for observability metrics. `ON CONFLICT DO UPDATE` (now used after
production-hardening changes) handles this gracefully.

---

## Implementation Order

```
A1: nullable unique fixes     — COMPLETE ✓
A3: artifact GC index         — COMPLETE ✓
A4: notification index sort   — COMPLETE ✓
A2: UUIDv7 for observe        — COMPLETE ✓
```

### Deviations

- **A1:** Postgres truncates the delegations constraint name to 63 chars.
  Actual name: `delegations_delegator_id_delegate_id_permission_id_project__key`.
  Used explicit short name `uq_delegations_unique_grant` for the replacement.
- **A2:** Used `Uuid::now_v7()` with iterator `.map(|_| Uuid::now_v7())` instead
  of a pre-allocated for loop.

All four are independent.

| Item | Migration | Code files | Test files |
|------|-----------|------------|------------|
| A1 | `030001_nullable_unique_fixes` | (none) | `tests/admin_integration.rs` |
| A2 | (none) | `Cargo.toml`, `src/observe/store.rs` | `tests/observe_integration.rs` |
| A3 | `030002_artifact_gc_index` | (none) | (none) |
| A4 | `030003_notification_index_sort` | (none) | (none) |

---

## Summary

| Finding | Severity | Action | Effort |
|---------|----------|--------|--------|
| #7 user_roles nullable unique | Medium | **A1** — migration | 10 min |
| #8 delegations nullable unique | Medium | **A1** — same migration | — |
| #9 UUIDv7 for observe | Medium | **A2** — Cargo + store.rs | 45 min |
| #10 artifact GC index | Medium | **A3** — migration | 5 min |
| #11 secrets scope overlap | Medium | **Skip** — non-issue after investigation | — |
| #12 notification sort index | Low | **A4** — migration | 5 min |
| #13–#20 | Low/Info | **Skip** — by design or future work | — |

Total: ~1.5 hours for all actionable items.
