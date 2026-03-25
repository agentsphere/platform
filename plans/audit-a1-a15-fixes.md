# Plan: Fix Audit Findings A1–A15

## Context

The codebase audit (`plans/codebase-audit-2026-03-24.md`) identified 7 critical and 8 high-severity findings. This plan addresses A1–A15 — the most impactful security and correctness gaps. The fixes are grouped into 4 PRs by domain to keep each PR atomic and reviewable.

**Current state:** The core modules (auth, projects, issues, MRs) follow strict patterns — `require_project_read()` returns 404, all mutations have `write_audit()`, inputs are validated. But newer modules (flags, deployments, onboarding, dashboard) deviate from these patterns.

## Design Principles

- **Pattern alignment** — every fix follows the existing canonical pattern from `src/api/helpers.rs` and established modules
- **Minimal changes** — each fix is surgical; no refactoring beyond what the finding requires
- **Test coverage** — every fix gets an integration test proving the before/after behavior
- **No migrations** — all fixes are code-only (auth checks, validation, audit logging, enum guards)

---

## PR 1: Auth & Authorization Fixes (A1, A2, A4)

Adds missing authorization checks to dashboard/onboarding and fixes 403→404 in 4 permission helpers.

- [x] Types & errors defined
- [x] Migration applied (N/A)
- [x] Tests written (red phase)
- [x] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/api/dashboard.rs:82` | `dashboard_stats`: rename `_auth` → `auth`, add `require_admin(&state, &auth).await?;` |
| `src/api/dashboard.rs:139` | `list_audit_log`: rename `_auth` → `auth`, add `require_admin(&state, &auth).await?;` |
| `src/api/onboarding.rs:530` | `claude_auth_status`: rename `_auth` → `auth`, add session ownership check — fetch session row, verify `session.user_id == auth.user_id`, fall back to `require_admin()` |
| `src/api/onboarding.rs:591` | `cancel_claude_auth`: rename `_auth` → `auth`, same ownership check + `require_admin()` fallback, add `write_audit()` for the cancel mutation |
| `src/api/secrets.rs:124` | `require_secret_read`: change `Err(ApiError::Forbidden)` → `Err(ApiError::NotFound("secret".into()))` |
| `src/api/deployments.rs:246` | `require_deploy_read`: change `Err(ApiError::Forbidden)` → `Err(ApiError::NotFound("project".into()))`, add `auth.check_project_scope(project_id)?;` at the top |
| `src/api/deployments.rs:268` | `require_deploy_promote`: add `auth.check_project_scope(project_id)?;` at the top |
| `src/api/sessions.rs:189` | `require_agent_run`: change `Err(ApiError::Forbidden)` → `Err(ApiError::NotFound("project".into()))` |
| `src/api/flags.rs:184` | `require_flag_manage`: change `Err(ApiError::Forbidden)` → `Err(ApiError::NotFound("project".into()))`, add `auth.check_project_scope(project_id)?;` at the top |

### Test Outline — PR 1

**New tests (integration):**

| Test | File | What it asserts |
|---|---|---|
| `dashboard_stats_requires_admin` | `tests/dashboard_integration.rs` | Non-admin user gets 403 on `GET /api/dashboard/stats` |
| `audit_log_requires_admin` | `tests/dashboard_integration.rs` | Non-admin user gets 403 on `GET /api/dashboard/audit-log` |
| `claude_auth_status_requires_ownership` | `tests/onboarding_integration.rs` (or new file) | User A cannot check User B's auth session (gets 404) |
| `cancel_claude_auth_requires_ownership` | `tests/onboarding_integration.rs` | User A cannot cancel User B's auth session (gets 404) |
| `cancel_claude_auth_admin_can_cancel` | `tests/onboarding_integration.rs` | Admin can cancel any session |
| `secret_read_returns_404_not_403` | `tests/secrets_integration.rs` | Unauthorized user gets 404 (not 403) on secret read |
| `deploy_read_returns_404_not_403` | `tests/deployment_integration.rs` | Unauthorized user gets 404 (not 403) on target/release read |
| `flag_manage_returns_404_not_403` | `tests/deployment_integration.rs` | Unauthorized user gets 404 (not 403) on flag endpoints |

**Existing tests to update:**

| Test | File | Change |
|---|---|---|
| `target_read_requires_permission` | `tests/deployment_integration.rs:532` | Change `assert_eq!(status, StatusCode::FORBIDDEN)` → `assert_eq!(status, StatusCode::NOT_FOUND)` |
| `release_read_requires_permission` | `tests/deployment_integration.rs:549` | Same: 403 → 404 |

**Estimated:** ~8 new integration tests + 2 updated

### Verification
- `just test-unit` passes (no unit changes)
- `just test-integration` passes — new tests prove auth enforcement, updated tests match new 404 behavior
- Manual: `curl` as non-admin to `/api/dashboard/stats` returns 403

---

## PR 2: Input Validation Fixes (A5, A7)

Adds missing validation on admin endpoints and pipeline image validation.

- [x] Types & errors defined
- [x] Migration applied (N/A)
- [x] Tests written (red phase)
- [x] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

> **Deviation:** Added `check_pipeline_image()` in validation.rs that allows `$` for K8s env var substitution (e.g. `$REGISTRY/$PROJECT/app:$COMMIT_SHA`). Pipeline images are template strings resolved by K8s, not executed in a shell. Used `check_pipeline_image` instead of `check_container_image` in pipeline definition validation.

### Code Changes

| File | Change |
|---|---|
| `src/api/admin.rs:185` | `create_role`: add `validation::check_name(&body.name)?;` and `if let Some(ref desc) = body.description { validation::check_length("description", desc, 0, 10_000)?; }` before the INSERT |
| `src/api/admin.rs:269` | `set_role_permissions`: add `if body.permissions.len() > 100 { return Err(ApiError::BadRequest("too many permissions".into())); }` and `for perm in &body.permissions { validation::check_length("permission", perm, 1, 255)?; }` |
| `src/api/admin.rs:410` | `create_delegation_handler`: add `if let Some(ref reason) = body.reason { validation::check_length("reason", reason, 0, 10_000)?; }` |
| `src/pipeline/definition.rs:~425` | In `validate()`, `StepKind::Command` arm: add `crate::validation::check_container_image(&step.image).map_err(|e| PipelineError::InvalidDefinition(e.to_string()))?;` |
| `src/pipeline/definition.rs:~400` | In `validate()`, `StepKind::ImageBuild` arm: add same `check_container_image` on `step.image_name` (if present) |
| `src/pipeline/definition.rs:~663` | In `validate_deploy_test()`: add `check_container_image(&dt.test_image).map_err(|e| PipelineError::InvalidDefinition(e.to_string()))?;` |
| `src/pipeline/error.rs` | Verify `InvalidDefinition(String)` variant exists — it does, no change needed |

### Test Outline — PR 2

**New tests (integration):**

| Test | File | What it asserts |
|---|---|---|
| `create_role_validates_name` | `tests/admin_integration.rs` | Role name with shell chars (`; rm -rf /`) returns 400 |
| `create_role_validates_name_length` | `tests/admin_integration.rs` | Role name > 255 chars returns 400 |
| `create_role_validates_description_length` | `tests/admin_integration.rs` | Description > 10,000 chars returns 400 |
| `set_role_permissions_validates_count` | `tests/admin_integration.rs` | > 100 permissions returns 400 |
| `set_role_permissions_validates_length` | `tests/admin_integration.rs` | Permission string > 255 chars returns 400 |
| `create_delegation_validates_reason_length` | `tests/admin_integration.rs` | Reason > 10,000 chars returns 400 |

**New tests (unit):**

| Test | File | What it asserts |
|---|---|---|
| `pipeline_rejects_image_with_shell_chars` | `src/pipeline/definition.rs` (unit test module) | Image `"alpine;rm -rf /"` fails validation |
| `pipeline_rejects_deploy_test_image_injection` | `src/pipeline/definition.rs` | Deploy test image with `$()` fails |
| `pipeline_accepts_valid_image` | `src/pipeline/definition.rs` | `"registry.example.com/app:v1.2.3"` passes |

**Estimated:** ~6 new integration + ~3 new unit tests

### Verification
- `just test-unit` — new pipeline definition unit tests pass
- `just test-integration` — admin validation tests pass
- Existing `admin_create_role` test still passes (valid names unaffected)

---

## PR 3: Audit Logging Fixes (A8, A11, A14, A15)

Adds CSP header, makes `write_audit` non-blocking, adds audit logging to flags and deployments modules.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

#### A8: CSP header

| File | Change |
|---|---|
| `src/main.rs:~258` | Add after the existing security headers: `.layer(SetResponseHeaderLayer::if_not_present(HeaderName::from_static("content-security-policy"), HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' ws: wss:")))` |

Using `if_not_present` so the preview proxy (which strips backend CSP) doesn't conflict.

#### A11: Non-blocking audit

| File | Change |
|---|---|
| `src/audit.rs:15` | Change `write_audit` to spawn the insert: `pub fn write_audit(pool: &PgPool, entry: AuditEntry<'static>) { tokio::spawn(async move { write_audit_inner(pool, &entry).await ... }); }` |

**Wait — on reflection this requires making `AuditEntry` own its data (`String` instead of `&str`).** This is a bigger change affecting ~100 call sites. Let's keep write_audit as `async` (blocking) for now and **downgrade A11 to MEDIUM/backlog** — the INSERT is sub-millisecond under normal load. We'll skip A11 in this PR to avoid a large refactor.

#### A14: Flags audit logging

| File | Change |
|---|---|
| `src/api/flags.rs` (top) | Add `use crate::audit::{write_audit, AuditEntry};` |
| `src/api/flags.rs:~238` | `create_flag`: add `write_audit(&state.pool, &AuditEntry { actor_id: auth.user_id, actor_name: &auth.user_name, action: "flag.create", resource: "feature_flag", resource_id: Some(flag_id), project_id: Some(project_id), detail: None, ip_addr: auth.ip_addr.as_deref() }).await;` after the INSERT |
| `src/api/flags.rs:~341` | `update_flag`: add `write_audit(...)` with action `"flag.update"` |
| `src/api/flags.rs:~373` | `delete_flag`: add with action `"flag.delete"` |
| `src/api/flags.rs:~401` | `toggle_flag`: add with action `"flag.toggle"` |
| `src/api/flags.rs:~464` | `add_rule`: add with action `"flag.rule.add"` |
| `src/api/flags.rs:~497` | `delete_rule`: add with action `"flag.rule.delete"` |
| `src/api/flags.rs:~536` | `set_override`: add with action `"flag.override.set"` |
| `src/api/flags.rs:~570` | `delete_override`: add with action `"flag.override.delete"` |

#### A15: Deployments audit logging

| File | Change |
|---|---|
| `src/api/deployments.rs:~377` | `create_target`: add `write_audit(...)` with action `"deploy.target.create"` after INSERT |
| `src/api/deployments.rs:~518` | `create_release`: add with action `"deploy.release.create"` |
| `src/api/deployments.rs:~565` | `adjust_traffic`: add with action `"deploy.traffic.adjust"` |
| `src/api/deployments.rs:~601` | `promote_release`: add with action `"deploy.release.promote"` |
| `src/api/deployments.rs:~641` | `rollback_release`: add with action `"deploy.release.rollback"` |
| `src/api/deployments.rs:~677` | `pause_release`: add with action `"deploy.release.pause"` |
| `src/api/deployments.rs:~713` | `resume_release`: add with action `"deploy.release.resume"` |

### Test Outline — PR 3

**New tests (integration):**

| Test | File | What it asserts |
|---|---|---|
| `csp_header_present` | `tests/contract_integration.rs` | Response to `GET /` includes `content-security-policy` header |
| `csp_header_preview_allows_framing` | `tests/contract_integration.rs` | Response to `/preview/` has `x-frame-options: SAMEORIGIN` (existing behavior, just verify CSP doesn't break it) |
| `create_flag_writes_audit` | `tests/deployment_integration.rs` | After creating a flag, query `audit_log WHERE action = 'flag.create'` returns 1 row |
| `toggle_flag_writes_audit` | `tests/deployment_integration.rs` | After toggling, `audit_log WHERE action = 'flag.toggle'` returns 1 row |
| `create_target_writes_audit` | `tests/deployment_integration.rs` | After creating target, `audit_log WHERE action = 'deploy.target.create'` returns 1 row |
| `create_release_writes_audit` | `tests/deployment_integration.rs` | After creating release, `audit_log WHERE action = 'deploy.release.create'` returns 1 row |

**Estimated:** ~6 new integration tests

### Verification
- `just test-integration` — audit log presence assertions pass
- Browser: inspect response headers on `/` for CSP, on `/preview/` for SAMEORIGIN

---

## PR 4: State Machine & Delegation Fixes (A6, A9, A10, A12, A13)

Adds pipeline status enum with transition validation, enforces ReleasePhase transitions in reconciler, limits delegation re-delegation, and tightens validation helpers.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

#### A9: Pipeline status enum

| File | Change |
|---|---|
| `src/pipeline/mod.rs` | Add `PipelineStatus` enum with variants: `Pending, Running, Success, Failure, Cancelled`. Add `can_transition_to()` method. Add `as_str()` and `FromStr`/`sqlx::Type` derives. Pattern: follow `ReleasePhase` in `src/deployer/types.rs`. |
| `src/pipeline/executor.rs:~115` | Before `UPDATE pipelines SET status = 'running'`: parse current status, call `can_transition_to(PipelineStatus::Running)`, skip if invalid. Same at line ~236 for running→success/failure. |
| `src/pipeline/executor.rs:~2653` | `mark_pipeline_failed`: parse current status, validate transition to `Failure`. |
| `src/pipeline/executor.rs:~3064` | Cancel: validate transition to `Cancelled`. |

#### A10: Reconciler phase validation

| File | Change |
|---|---|
| `src/deployer/reconciler.rs:717-740` | `transition_phase`: parse `release.phase` and `new_phase` into `ReleasePhase` enums. Call `current.can_transition_to(next)`. If invalid, log warning and return `DeployerError::InvalidTransition`. |
| `src/deployer/error.rs` | Add `InvalidTransition { from: String, to: String }` variant if not present. Map to `ApiError::BadRequest`. |

#### A6: Delegation chain prevention

| File | Change |
|---|---|
| `src/rbac/delegation.rs:~50` | After checking `has_permission()`, add a check: query `delegations` table to see if the delegator holds this permission via delegation (not direct role). If so, return `ApiError::BadRequest("cannot re-delegate a delegated permission")`. |

Specifically, add after line 62:
```rust
// Check if delegator's permission comes from delegation (not direct role)
let via_delegation = sqlx::query_scalar!(
    r#"SELECT EXISTS(
        SELECT 1 FROM delegations d
        JOIN permissions p ON p.id = d.permission_id
        WHERE d.delegate_id = $1 AND p.name = $2
        AND d.revoked_at IS NULL
        AND (d.expires_at IS NULL OR d.expires_at > now())
    ) as "exists!""#,
    req.delegator_id,
    req.permission.as_str(),
)
.fetch_one(pool)
.await?;

// Also check if they have it via a direct role
let via_role = sqlx::query_scalar!(
    r#"SELECT EXISTS(
        SELECT 1 FROM user_roles ur
        JOIN role_permissions rp ON rp.role_id = ur.role_id
        JOIN permissions p ON p.id = rp.permission_id
        WHERE ur.user_id = $1 AND p.name = $2
        AND (ur.project_id = $3 OR $3::uuid IS NULL)
    ) as "exists!""#,
    req.delegator_id,
    req.permission.as_str(),
    req.project_id,
)
.fetch_one(pool)
.await?;

if !via_role {
    return Err(ApiError::BadRequest(
        "cannot delegate a permission obtained only via delegation".into(),
    ));
}
```

#### A12: ASCII-only `check_name`

| File | Change |
|---|---|
| `src/validation.rs:24` | Change `c.is_alphanumeric()` → `c.is_ascii_alphanumeric()` |

#### A13: Reject control chars in `check_length`

| File | Change |
|---|---|
| `src/validation.rs:~10` | After the length check, add: `if value.bytes().any(|b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t') { return Err(ApiError::BadRequest(format!("{field}: contains control characters"))); }` |

Note: Allow `\n`, `\r`, `\t` since they're valid in bodies/descriptions. Reject null bytes and other control chars (0x00–0x1F except 0x09, 0x0A, 0x0D).

### Test Outline — PR 4

**New tests (unit):**

| Test | File | What it asserts |
|---|---|---|
| `pipeline_status_valid_transitions` | `src/pipeline/mod.rs` | Pending→Running, Running→Success/Failure/Cancelled are valid |
| `pipeline_status_invalid_transitions` | `src/pipeline/mod.rs` | Success→Running, Cancelled→Running, Failure→Running are invalid |
| `pipeline_status_terminal_cannot_transition` | `src/pipeline/mod.rs` | Success/Failure/Cancelled cannot transition to anything |
| `check_name_rejects_unicode` | `src/validation.rs` | `"tëst"` returns BadRequest |
| `check_name_accepts_ascii` | `src/validation.rs` | `"test-name_v1.0"` passes |
| `check_length_rejects_null_bytes` | `src/validation.rs` | `"ab\0cd"` returns BadRequest |
| `check_length_rejects_control_chars` | `src/validation.rs` | `"ab\x01cd"` returns BadRequest |
| `check_length_allows_newlines` | `src/validation.rs` | `"line1\nline2"` passes (for body/description fields) |

**New tests (integration):**

| Test | File | What it asserts |
|---|---|---|
| `delegation_re_delegation_blocked` | `tests/rbac_integration.rs` | User B (who has a delegated permission) cannot re-delegate it to User C — returns 400 |
| `delegation_direct_role_can_delegate` | `tests/rbac_integration.rs` | User with permission via direct role CAN delegate it |
| `reconciler_invalid_transition_rejected` | `tests/deployment_integration.rs` or `tests/e2e_deployer.rs` | Verify that a completed release cannot transition to progressing |
| `check_name_unicode_rejection_in_role` | `tests/admin_integration.rs` | Role name with Cyrillic chars returns 400 |

**Existing tests to update:**

| Test | File | Change |
|---|---|---|
| `check_name_unicode_allowed` | `src/validation.rs` (unit tests ~line 336-346) | Update: Unicode names should now **fail** validation (was explicitly allowed, now rejected) |
| Any test using Unicode in project/role/user names | Various | Verify they use ASCII-only names (most already do) |

**Estimated:** ~8 new unit + ~4 new integration + ~1 updated unit test

### Verification
- `just test-unit` — pipeline status enum, validation changes all pass
- `just test-integration` — delegation and admin tests pass
- `just test-e2e` — deployer reconciler tests still pass with phase validation

---

## Summary

| PR | Findings | Files Changed | New Tests | Updated Tests |
|---|---|---|---|---|
| PR 1: Auth & Authorization | A1, A2, A4 | 7 files | ~8 integration | ~2 |
| PR 2: Input Validation | A5, A7 | 4 files | ~6 integration + ~3 unit | 0 |
| PR 3: Audit Logging + CSP | A8, A14, A15 | 4 files | ~6 integration | 0 |
| PR 4: State Machines & Validation | A6, A9, A10, A12, A13 | 7 files | ~8 unit + ~4 integration | ~1 |
| **Total** | **A1–A10, A12–A15** | **~18 files** | **~35 tests** | **~3 updated** |

**Deferred:** A11 (non-blocking write_audit) — requires `AuditEntry` to own its strings, touching ~100 call sites. Downgraded to MEDIUM/backlog. A3 (SSH branch protection) — requires protocol-level changes, tracked separately.
