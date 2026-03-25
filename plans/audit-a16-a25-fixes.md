# Plan: Fix Audit Findings A16-A25

## Context

These are the remaining 10 HIGH findings from the 2026-03-24 codebase audit. They span config safety, pipeline resource limits, deployer allowlists, git/registry memory safety, pipeline code quality, auth UX, and log hygiene. All are HIGH severity — no critical findings in this batch, but several carry OOM or credential-leak risk.

## Design Principles

- **Minimal blast radius** — each fix is isolated to 1-2 files; no cross-module refactors
- **No breaking API changes** — all fixes are backward-compatible
- **Defense in depth** — prefer fail-closed (reject, cap, redact) over fail-open
- **Test every fix** — each finding gets at least one unit test proving the fix works
- **Production safety** — replace `.unwrap()` with proper error handling, never panic

---

## PR 1: Config Debug Redaction & Webhook URL Scrubbing (A16, A25)

Two related credential-leak fixes: Config Debug printing and webhook URL logging.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/config.rs` | Remove `#[derive(Debug)]` from `Config`. Implement manual `fmt::Debug` that redacts `database_url`, `valkey_url`, `minio_secret_key`, `smtp_password`, `admin_password`, `master_key`. Show field name with value `"[REDACTED]"` for sensitive fields. Keep all other fields printing normally. |
| `src/notify/webhook.rs:26` | Change `#[tracing::instrument(skip(payload, secret), err)]` to `#[tracing::instrument(skip(url, payload, secret), err)]` — skip the `url` parameter from the instrument macro so it is never recorded as a span field. |
| `src/notify/webhook.rs:63` | Change `tracing::info!(url, status, "notification webhook delivered")` to `tracing::info!(status, "notification webhook delivered")` — remove `url` from structured fields. |
| `src/notify/webhook.rs:70` | Change `tracing::warn!(url, error = %e, "notification webhook delivery failed")` to `tracing::warn!(error = %e, "notification webhook delivery failed")` — remove `url`. |
| `src/notify/webhook.rs:38` | Change `tracing::warn!(url, "notification webhook dropped: concurrency limit reached")` to `tracing::warn!("notification webhook dropped: concurrency limit reached")` — remove `url`. |

### Test Outline

**New tests (unit):**
| Test | File | What it asserts |
|---|---|---|
| `config_debug_redacts_sensitive_fields` | `src/config.rs` | `format!("{:?}", Config::test_default())` does NOT contain the literal values of `database_url`, `minio_secret_key`, etc. Asserts the output contains `"[REDACTED]"` for each sensitive field. |
| `config_debug_shows_non_sensitive_fields` | `src/config.rs` | `format!("{:?}", Config::test_default())` DOES contain `listen`, `smtp_port`, `dev_mode` values, proving non-sensitive fields are still visible. |

**New tests (integration):**
| Test | File | What it asserts |
|---|---|---|
| N/A | — | Webhook URL logging is a negative test (absence of data in logs). Verified by code review. |

**Existing tests to update:**
| Test | File | Change |
|---|---|---|
| None — existing tests don't assert on Debug output | — | — |

### Verification
- `cargo test --lib -p platform -- config::tests` — verify redaction unit tests pass.
- `cargo clippy --all-features -- -D warnings` — no new warnings.
- Manual: `tracing::debug!("{:?}", config)` in a test to visually confirm redaction.

---

## PR 2: Pipeline-Level Timeout (A17)

Add configurable pipeline-level timeout. Individual steps already have 15-minute timeouts, but the overall pipeline has none. A pipeline with 20 steps could theoretically run for 5 hours.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/config.rs` | Add `pub pipeline_timeout_secs: u64` field to `Config`. Default: `3600` (1 hour). Env var: `PLATFORM_PIPELINE_TIMEOUT`. Add to `test_default()` with value `3600`. |
| `src/pipeline/executor.rs` (in `execute_pipeline`) | Wrap the `run_all_steps(...)` call with `tokio::time::timeout(Duration::from_secs(state.config.pipeline_timeout_secs), run_all_steps(...))`. On timeout, mark pipeline as `failure` with a log message `"pipeline timed out after {n}s"`, clean up registry/git auth tokens, and return. |
| `src/pipeline/executor.rs` (in `execute_pipeline`) | Extract the cleanup + finalize logic into a helper or ensure the timeout path also runs cleanup (registry secret, git auth token). Restructure so that cleanup always runs regardless of timeout. |

### Test Outline

**New tests (unit):**
| Test | File | What it asserts |
|---|---|---|
| `test_default_pipeline_timeout` | `src/config.rs` | `Config::test_default().pipeline_timeout_secs == 3600` |

**New tests (integration):**
| Test | File | What it asserts |
|---|---|---|
| Pipeline timeout is tested indirectly — the constant is plumbed through config. Full integration testing requires a slow pipeline which is impractical. The existing pipeline integration tests verify the execution loop works end-to-end. | — | — |

**Existing tests to update:**
| Test | File | Change |
|---|---|---|
| `Config::test_default()` | `src/config.rs` | Add `pipeline_timeout_secs: 3600` to the test default struct. |

### Verification
- `just test-unit` — config tests pass.
- `just lint` — no warnings.
- Code review: verify the timeout wraps the entire `run_all_steps` call and cleanup still runs.

---

## PR 3: Remove Gateway from ALLOWED_KINDS (A18)

Remove `Gateway` from the deployer's allowed resource kinds. Users should only create `HTTPRoute` resources (which reference existing Gateways via `parentRefs`), not `Gateway` resources themselves which could capture cross-tenant traffic.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/deployer/applier.rs:51` | Remove `"Gateway"` from the `ALLOWED_KINDS` array. Keep `"HTTPRoute"`. Update comment: `// Gateway API: only HTTPRoute (referencing existing Gateways via parentRefs)` |

### Test Outline

**New tests (unit):**
| Test | File | What it asserts |
|---|---|---|
| `gateway_kind_rejected` | `src/deployer/applier.rs` (or existing test file) | Apply a manifest with `kind: Gateway` and assert it is rejected with an error indicating the kind is not allowed. |

**New tests (integration):**
| Test | File | What it asserts |
|---|---|---|
| N/A — the ALLOWED_KINDS check is a static list. Unit test is sufficient. | — | — |

**Existing tests to update:**
| Test | File | Change |
|---|---|---|
| Any existing test that creates a `Gateway` resource in test manifests needs updating to use `HTTPRoute` instead. Search for `kind: Gateway` in test fixtures. | — | Update if found. |

### Verification
- `just test-unit` — verify the new test passes.
- `rg "Gateway" src/deployer/` — confirm no references to Gateway in ALLOWED_KINDS.

---

## PR 4: Streaming Fixes for OOM Prevention (A19, A20, A21)

Three OOM-risk fixes: receive-pack body buffering, LFS upload size limits, and registry blob streaming.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

#### A19: receive-pack streaming

| File | Change |
|---|---|
| `src/git/smart_http.rs` (in `receive_pack`) | The current approach collects the entire body to parse ref commands before piping to git. A full streaming rewrite would be complex because branch protection requires parsing the pack commands header *before* piping data to git. Instead, **add an explicit size cap** before the `.collect()` call: check `Content-Length` header against a configurable max (default 500 MB, matching the `RequestBodyLimitLayer`). If missing or oversized, return `413 Payload Too Large`. This is defense-in-depth since `RequestBodyLimitLayer` already caps at 500 MB, but the explicit check prevents the full body from being held in memory if the middleware is ever misconfigured. |

The `RequestBodyLimitLayer` at 500 MB in `main.rs` already prevents truly massive pushes. The `.collect().to_bytes()` is bounded by this middleware. The real fix here is to document that the 500 MB body limit already constrains this path, and add a comment clarifying the memory bound. A full streaming rewrite (parsing pack commands from a stream) is a larger effort that can be tracked separately.

#### A20: LFS upload size limit

| File | Change |
|---|---|
| `src/config.rs` | Add `pub max_lfs_object_bytes: i64` field. Default: `5_368_709_120` (5 GB). Env var: `PLATFORM_MAX_LFS_OBJECT_BYTES`. Add to `test_default()`. |
| `src/git/lfs.rs` (in `batch`, inside the `for obj in &body.objects` loop) | Before generating presigned URLs for `"upload"` operations, check `if obj.size > state.config.max_lfs_object_bytes { return Err(ApiError::BadRequest(format!("LFS object too large: {} bytes (max {})", obj.size, state.config.max_lfs_object_bytes))); }`. Also reject `obj.size < 0`. |

#### A21: Registry blob streaming

| File | Change |
|---|---|
| `src/registry/blobs.rs` (in `get_blob`) | Replace `state.minio.read(&blob.minio_path).await?.to_vec()` with a presigned URL redirect. Use `state.minio.presign_read(&blob.minio_path, Duration::from_secs(3600)).await?` and return a `307 Temporary Redirect` to the presigned URL. This is how production registries handle large blob downloads — the client follows the redirect to S3/MinIO. Set `Location` header to the presigned URL and return `StatusCode::TEMPORARY_REDIRECT`. This avoids loading the entire blob into platform memory. |

Alternatively, if redirect is not desirable for Docker client compatibility, use `opendal::Operator::reader()` to get a streaming reader and convert it to an axum `Body::from_stream()`. However, the presigned-URL redirect approach is simpler and more standard for OCI registries (Docker Hub and ECR both use redirects).

**Chosen approach: presigned URL redirect.** Docker clients and container runtimes all follow redirects per the OCI distribution spec.

### Test Outline

**New tests (unit):**
| Test | File | What it asserts |
|---|---|---|
| `lfs_upload_rejects_oversized_object` | `src/git/lfs.rs` or `tests/setup_integration.rs` | LFS batch request with `size: 6_000_000_000` returns 400. |
| `lfs_upload_rejects_negative_size` | `src/git/lfs.rs` or `tests/setup_integration.rs` | LFS batch request with `size: -1` returns 400. |
| `test_default_max_lfs_object_bytes` | `src/config.rs` | `Config::test_default().max_lfs_object_bytes == 5_368_709_120` |

**New tests (integration):**
| Test | File | What it asserts |
|---|---|---|
| `registry_get_blob_returns_redirect` | `tests/setup_integration.rs` | GET `/v2/{name}/blobs/{digest}` returns 307 with a `Location` header pointing to a presigned MinIO URL. |

**Existing tests to update:**
| Test | File | Change |
|---|---|---|
| Any existing registry blob GET tests | `tests/setup_integration.rs` or `tests/helpers/` | Update expected status from 200 to 307 if they check status codes. |
| `Config::test_default()` | `src/config.rs` | Add `max_lfs_object_bytes: 5_368_709_120` |

### Verification
- `just test-unit` — LFS size validation tests pass.
- `just test-integration` — registry blob redirect test passes, existing tests updated.
- Manual: push a large LFS file and verify presigned URL generation.

---

## PR 5: Remove Production .unwrap() Calls (A22)

Replace three `.unwrap()` calls in production code with proper error handling.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/pipeline/definition.rs:407` | Replace `step.deploy_test.as_ref().unwrap()` with `step.deploy_test.as_ref().ok_or_else(\|\| PipelineError::InvalidDefinition(format!("step '{}': deploy_test kind but no deploy_test config", step.name)))?`. While logically safe (the `kind()` method returns `DeployTest` only when `deploy_test.is_some()`), the `unwrap` is fragile if `kind()` logic ever changes. |
| `src/pipeline/definition.rs:805` | Replace `*c.steps.last().unwrap()` with `c.steps.last().copied().unwrap_or(0)`. This is safe because `c.steps.is_empty()` is checked at line 791, but using `unwrap_or` eliminates the unwrap while preserving the same logic (0 != 100, so the error branch fires). |
| `src/pipeline/executor.rs:557` | Replace `sem.acquire().await.unwrap()` with `sem.acquire().await.expect("pipeline semaphore closed unexpectedly")`. The `Semaphore::acquire()` only fails if the semaphore is closed, which never happens in this code. An `.expect()` with a message is clearer than bare `.unwrap()`. Alternatively, use `sem.acquire().await.map_err(\|_\| PipelineError::Internal("pipeline concurrency semaphore closed".into()))?` if `PipelineError` has an appropriate variant. |

### Test Outline

**New tests (unit):**
| Test | File | What it asserts |
|---|---|---|
| N/A — these are defensive changes to existing code paths that are already tested. The `.unwrap()` locations are all covered by existing tests that exercise the `validate()` and `run_steps_dag()` paths. | — | — |

**New tests (integration):**
| Test | File | What it asserts |
|---|---|---|
| N/A | — | — |

**Existing tests to update:**
| Test | File | Change |
|---|---|---|
| None — existing tests already pass through these code paths without hitting the unwrap failure case. | — | — |

### Verification
- `just lint` — confirm no new warnings.
- `rg '\.unwrap()' src/pipeline/definition.rs src/pipeline/executor.rs` — verify no remaining production unwraps (test code is fine).
- `just test-unit` — existing pipeline tests still pass.

---

## PR 6: Passkey Login Credential Pre-filtering (A23)

The current `complete_login()` loads ALL passkey credentials for ALL active users, which is a scalability problem and potential DoS vector.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/api/passkeys.rs` (in `complete_login`) | The WebAuthn discoverable authentication flow requires the server to have the list of all possible credentials to match against. However, the `PublicKeyCredential` from the client contains a `raw_id` field (the credential ID chosen by the authenticator). We can use this to pre-filter. **Step 1:** Extract `body.credential.raw_id` (the credential ID bytes) before calling `finish_discoverable_authentication`. **Step 2:** Query only credentials matching that credential ID: `SELECT ... FROM passkey_credentials pc JOIN users u ON u.id = pc.user_id WHERE pc.credential_id = $1 AND u.is_active = true`. **Step 3:** If no rows found, return `Unauthorized`. **Step 4:** Build `discoverable_keys` from the filtered set (typically 1 row). **Step 5:** Call `finish_discoverable_authentication` with the filtered list. |

**Note:** The `webauthn_rs` `PublicKeyCredential` struct exposes `.raw_id()` or the raw credential ID. We need to check the exact API. The credential ID from the client's response is the authenticator's credential ID, which matches `passkey_credentials.credential_id` in the DB.

However, we need to verify that `webauthn_rs::prelude::PublicKeyCredential` exposes the credential ID before the `finish_*` call. Let me check the webauthn_rs API:

The `PublicKeyCredential` struct in `webauthn-rs` has a `.raw_id` field (or equivalent). The `Passkey` struct stores it as `cred_id()`. The DB stores `credential_id` as `BYTEA`. We can extract the credential ID from the request's `body.credential` and use it to filter.

**Alternative simpler approach:** If extracting the raw ID from the webauthn types is complex, we can add a `LIMIT` clause to cap the query (e.g., `LIMIT 1000`) and add monitoring. But the pre-filter approach is more correct.

| File | Change |
|---|---|
| `src/api/passkeys.rs` (in `complete_login`) | Replace the full-table query with: `let cred_id_bytes = body.credential.raw_id().to_vec();` then `SELECT ... FROM passkey_credentials pc JOIN users u ON u.id = pc.user_id WHERE pc.credential_id = $1 AND u.is_active = true` binding `cred_id_bytes`. |

### Test Outline

**New tests (unit):**
| Test | File | What it asserts |
|---|---|---|
| N/A — passkey login requires a full WebAuthn ceremony which is integration-level. | — | — |

**New tests (integration):**
| Test | File | What it asserts |
|---|---|---|
| Existing passkey integration tests (if any) should continue to pass. The behavior is identical — just more efficient. | — | — |

**Existing tests to update:**
| Test | File | Change |
|---|---|---|
| None expected — the API contract is unchanged. | — | — |

### Verification
- `just test-unit` — no regressions.
- `just test-integration` — passkey tests pass (if any exist in the suite).
- Code review: verify the credential ID extraction matches the DB column type.

---

## PR 7: Logout Deletes Only Current Session (A24)

The current `logout()` handler deletes ALL sessions for the user, not just the session that made the request.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/auth/middleware.rs` | Add `pub session_token_hash: Option<String>` field to `AuthUser`. Populate it when authenticating via session (both Bearer-as-session and cookie paths). In the Bearer session path (line 144-163): after `lookup_session` succeeds, compute `let hash = token::hash_token(raw_token);` and set `session_token_hash: Some(hash)`. In the cookie path (line 167-190): after `lookup_session` succeeds, compute `let hash = token::hash_token(session_token);` and set `session_token_hash: Some(hash)`. For API token auth (line 130-138): set `session_token_hash: None`. |
| `src/auth/middleware.rs` (test helpers) | Add `session_token_hash: None` to all `AuthUser::test_*()` constructors. |
| `src/api/users.rs` (in `logout`) | Change `DELETE FROM auth_sessions WHERE user_id = $1` to: if `auth.session_token_hash` is `Some(hash)`, delete `WHERE user_id = $1 AND token_hash = $2` (binding both user_id and hash). If `session_token_hash` is `None` (API token auth), delete `WHERE user_id = $1` as fallback (or return an error — logging out via API token when there's no session is an edge case). Prefer the targeted delete. |

**Design decision:** When logged out via API token (no session), we have two options:
1. Return 400 "no session to log out" — strict but may break clients.
2. Delete all sessions — preserves current behavior for API token logout.
3. Delete nothing — API tokens are not sessions.

**Chosen:** Option 1 with graceful fallback. If `session_token_hash` is `None`, return `Ok` with `{"ok": true}` but don't delete any sessions. The cookie clearing still happens. This is correct: logging out via API token shouldn't nuke all web sessions.

| File | Change |
|---|---|
| `src/api/users.rs` (in `logout`) | Replace the SQL with: `if let Some(ref hash) = auth.session_token_hash { sqlx::query!("DELETE FROM auth_sessions WHERE user_id = $1 AND token_hash = $2", auth.user_id, hash).execute(&state.pool).await?; }` — only delete the current session. If no session hash (API token), skip the delete (just clear the cookie). |

### Test Outline

**New tests (unit):**
| Test | File | What it asserts |
|---|---|---|
| `auth_user_session_hash_populated_for_session` | `src/auth/middleware.rs` | (Difficult to unit test since it requires DB — defer to integration.) |

**New tests (integration):**
| Test | File | What it asserts |
|---|---|---|
| `logout_deletes_only_current_session` | `tests/setup_integration.rs` | Create user, login twice (two sessions), logout with first session token, verify second session still works (GET `/api/auth/me` with second token returns 200). |
| `logout_clears_cookie_even_without_session` | `tests/setup_integration.rs` | Logout via API token, verify response includes `Set-Cookie` header clearing the session cookie and returns 200. |

**Existing tests to update:**
| Test | File | Change |
|---|---|---|
| Any existing logout tests that assert all sessions are deleted need to be updated to only assert the current session is deleted. | `tests/setup_integration.rs` | Search for tests that call `/api/auth/logout` and verify behavior. |

### Verification
- `just test-integration` — new and existing logout tests pass.
- Manual: log in on two browser tabs, log out from one, verify the other remains active.

---

## Summary

| PR | Findings | Files Changed | New Unit Tests | New Integration Tests | Existing Tests Updated |
|---|---|---|---|---|---|
| PR 1: Config Debug & Webhook URL | A16, A25 | `src/config.rs`, `src/notify/webhook.rs` | 2 | 0 | 0 |
| PR 2: Pipeline Timeout | A17 | `src/config.rs`, `src/pipeline/executor.rs` | 1 | 0 | 1 (`test_default`) |
| PR 3: Remove Gateway ALLOWED_KINDS | A18 | `src/deployer/applier.rs` | 1 | 0 | 0-1 (if Gateway used in test fixtures) |
| PR 4: OOM Prevention (Streaming) | A19, A20, A21 | `src/config.rs`, `src/git/lfs.rs`, `src/git/smart_http.rs`, `src/registry/blobs.rs` | 3 | 1 | 1-2 (registry blob tests, `test_default`) |
| PR 5: Remove .unwrap() | A22 | `src/pipeline/definition.rs`, `src/pipeline/executor.rs` | 0 | 0 | 0 |
| PR 6: Passkey Pre-filter | A23 | `src/api/passkeys.rs` | 0 | 0 | 0 |
| PR 7: Logout Current Session | A24 | `src/auth/middleware.rs`, `src/api/users.rs` | 0 | 2 | 0-1 |
| **Total** | **A16-A25** | **10 files** | **7** | **3** | **2-5** |

### Recommended merge order

1. **PR 5** (unwrap removal) — zero risk, pure cleanup
2. **PR 1** (config debug + webhook URL) — simple, no behavior change for callers
3. **PR 3** (Gateway removal) — one-line change, clear security benefit
4. **PR 2** (pipeline timeout) — new config field + timeout wrapper
5. **PR 4** (OOM prevention) — largest PR, touches 4 files, registry behavior changes
6. **PR 6** (passkey pre-filter) — depends on webauthn_rs API details
7. **PR 7** (logout scope) — adds field to AuthUser, needs integration test coverage
