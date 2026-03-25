# Codebase Audit Report

**Date:** 2026-03-24
**Scope:** Full `src/` directory — 113 files, ~72K LOC, 16 modules
**Auditor:** Claude Code (automated, 12 parallel agents)
**Pre-flight:** fmt — has uncommitted diffs | lint — 2 clippy errors (cast_possible_truncation) | deny — not run | unit tests — not run (pre-flight non-blocking)

## Executive Summary

- **Overall health: NEEDS ATTENTION** — The codebase is well-structured with strong foundational security (argon2, SHA-256 token hashing, SSRF protection, unsafe forbidden), but has accumulated gaps in newer modules (flags, deployments, onboarding) that lack the same rigor applied to earlier modules (auth, projects, issues).
- **Findings:** 7 critical, 19 high, 42 medium, ~80 low
- **Top risks:** Unauthenticated dashboard/audit-log access; SSH push bypasses branch protection; IDOR in onboarding auth sessions; delegation chain enables unbounded privilege spreading
- **Strengths:** Timing-safe auth; comprehensive SSRF protection; bounded channels; proper shutdown signals; env_clear on CLI subprocesses; RBAC with workspace-derived permissions

## Statistics

| Module | Files | ~LOC | Critical | High | Medium | Low |
|---|---|---|---|---|---|---|
| api/ | 30 | ~18K | 4 | 10 | 14 | 25 |
| agent/ | 23 | ~14K | 0 | 0 | 3 | 13 |
| deployer/ | 11 | ~8K | 0 | 0 | 4 | 7 |
| pipeline/ | 5 | ~8K | 0 | 2 | 3 | 4 |
| git/ | 12 | ~6K | 1 | 1 | 1 | 5 |
| registry/ | 11 | ~5K | 0 | 0 | 1 | 3 |
| observe/ | 10 | ~5K | 0 | 0 | 2 | 5 |
| auth/ + rbac/ | 12 | ~3K | 0 | 0 | 2 | 4 |
| store/ | 6 | ~3K | 0 | 0 | 1 | 2 |
| secrets/ | 5 | ~2K | 0 | 0 | 1 | 2 |
| notify/ | 4 | ~1K | 0 | 0 | 1 | 2 |
| workspace/ | 3 | ~1K | 0 | 0 | 1 | 2 |
| foundation | 7 | ~3K | 0 | 0 | 4 | 3 |
| cross-cutting | — | — | 2 | 6 | 4 | 8 |
| **Total** | **113** | **~72K** | **7** | **19** | **42** | **~80** |

## Strengths

1. **Timing-safe authentication** — Argon2 with `dummy_hash()` for missing users prevents user enumeration. Salt via `argon2::password_hash::rand_core::OsRng`. SHA-256 token hashing with DB-level comparison.
2. **Comprehensive SSRF protection** — `check_ssrf_url()` blocks private IPs, link-local, loopback, cloud metadata, non-HTTP schemes. Applied to all webhook URLs.
3. **Bounded channels everywhere** — All `mpsc::channel` calls have explicit capacity (10K for observe, 256 for SSE, 64/16 for internal). No `unbounded_channel` usage found.
4. **Graceful shutdown** — `watch::channel` shutdown signal propagated to all background tasks (except session cleanup — finding A25). Proper `tokio::select!` usage throughout.
5. **Environment isolation for agent pods** — `env_clear()` + whitelist approach, `RESERVED_ENV_VARS` protection against secret injection, custom `Debug` impl redacting credentials, non-root pod security context.
6. **RBAC with workspace-derived permissions** — Workspace membership grants implicit project access, correctly cached with proper invalidation on role changes.
7. **No unsafe code** — `unsafe_code = "forbid"` in Cargo.toml lints, verified. OpenSSL banned in `deny.toml`.
8. **Server-side apply for K8s** — Idempotent `ALLOWED_KINDS` allowlist, namespace override enforcement, proper field manager usage.
9. **Idempotent bootstrap** — `ON CONFLICT DO NOTHING` for seed data, user count check before admin creation, dev-mode gating for default credentials.
10. **Container image validation** — `check_container_image()` blocks shell injection characters. Applied in API endpoints (but not pipeline YAML — see A12).

---

## Critical & High Findings (must address)

### A1: [CRITICAL] Dashboard stats and audit log accessible by any authenticated user
- **Module:** api/dashboard
- **File:** `src/api/dashboard.rs:82-191`
- **Description:** `dashboard_stats()` and `list_audit_log()` use `_auth: AuthUser` (underscore prefix) with no permission check. Any authenticated user can view platform-wide counts (total projects, active sessions, builds) and the entire audit log (actor names, actions, resource IDs, detail JSON for all users).
- **Risk:** Information disclosure of cross-tenant operational data and complete audit trail.
- **Suggested fix:** Add `require_admin(&state, &auth).await?;` at the top of both handlers.
- **Found by:** Agent 2

### A2: [CRITICAL] Onboarding IDOR — any user can check/cancel anyone's Claude auth session
- **Module:** api/onboarding
- **File:** `src/api/onboarding.rs:530-598`
- **Description:** `claude_auth_status()` and `cancel_claude_auth()` use `_auth: AuthUser` without verifying the session belongs to the requesting user. Any authenticated user can check the status of or cancel any other user's Claude CLI auth session by guessing/enumerating session IDs.
- **Risk:** Denial of service (cancelling other users' auth sessions) and information disclosure (session status).
- **Suggested fix:** Look up the session, verify `session.user_id == auth.user_id`, or require admin.
- **Found by:** Agent 2

### A3: [CRITICAL] SSH push bypasses branch protection entirely
- **Module:** git/ssh_server
- **File:** `src/git/ssh_server.rs:214-219`
- **Description:** The SSH `exec_request` handler calls `check_access_for_user` for RBAC but never calls `enforce_push_protection`. The HTTP `receive_pack` handler (`smart_http.rs:472`) enforces branch protection rules, but the SSH path does not. A user with write access can force-push to protected branches or push directly to branches that require PRs — by using SSH instead of HTTP.
- **Risk:** Complete bypass of branch protection rules for any user with SSH access and write permission.
- **Suggested fix:** Add branch protection enforcement in the SSH push path. Consider a server-side pre-receive hook that calls back to the platform API for protection checks.
- **Found by:** Agent 8

### A4: [CRITICAL] Multiple permission helpers return 403 instead of 404
- **Module:** api (secrets, deployments, sessions, flags)
- **Files:** `src/api/secrets.rs:106-127`, `src/api/deployments.rs:228-264`, `src/api/sessions.rs:188-189`, `src/api/flags.rs:182-184`
- **Description:** Permission helpers in 4 modules return `ApiError::Forbidden` instead of `ApiError::NotFound` for private resources. This leaks resource existence to unauthorized users, violating the project's security convention (CLAUDE.md: "return 404 not 403 for private resources").
- **Risk:** Resource existence enumeration across secrets, deployments, sessions, and feature flags.
- **Suggested fix:** Change `Err(ApiError::Forbidden)` to `Err(ApiError::NotFound("resource".into()))` in all affected helpers.
- **Found by:** Agents 1, 2

### A5: [CRITICAL] Missing input validation on admin role creation and delegation
- **Module:** api/admin
- **File:** `src/api/admin.rs:179-434`
- **Description:** `create_role()` has no validation on role name or description. `set_role_permissions()` has no validation on permission name strings. `create_delegation_handler()` has no validation on reason length. These are admin endpoints but an admin account compromise allows injecting arbitrary data.
- **Risk:** Storage of arbitrary/malicious strings in roles and permissions, potential XSS if displayed in UI.
- **Suggested fix:** Add `validation::check_name(&body.name)?`, `check_length("description", ...)`, validate permissions against known `Permission` enum variants, `check_length("reason", &reason, 0, 10_000)?`.
- **Found by:** Agent 3

### A6: [CRITICAL] Delegation chain allows unbounded privilege spreading
- **Module:** rbac/delegation
- **File:** `src/rbac/delegation.rs:37-106`
- **Description:** User A (admin) delegates `AdminUsers` to User B. User B can re-delegate to User C without restriction. There is no depth limit, no "delegated-only" flag preventing re-delegation, and no check whether the delegator's permission was itself obtained via delegation. This enables unbounded lateral permission spreading from a single delegation act.
- **Risk:** A single delegation can lead to organization-wide privilege escalation through transitive re-delegation.
- **Suggested fix:** Prevent re-delegation of delegated permissions by checking whether the delegator holds the permission via a direct role assignment (not another delegation), or add a `delegatable` flag.
- **Found by:** Agent 4

### A7: [CRITICAL] Missing `check_container_image()` on pipeline YAML images
- **Module:** pipeline/definition
- **File:** `src/pipeline/definition.rs:384-429`
- **Description:** The `validate()` function checks that `step.image` is non-empty for command steps, but never calls `check_container_image()` to reject shell injection characters. The validation exists in `src/validation.rs` and is used by `src/api/sessions.rs`, but not in the pipeline YAML parser. An attacker who controls `.platform.yaml` could craft an image string containing injection payloads.
- **Risk:** Shell injection via malicious container image names in pipeline definitions.
- **Suggested fix:** Add `crate::validation::check_container_image(&step.image)?` inside the `StepKind::Command` arm of `validate()`, and similarly for `deploy_test.test_image`.
- **Found by:** Agent 6

### A8: [HIGH] No Content-Security-Policy header
- **Module:** ui
- **File:** `src/ui.rs`
- **Description:** The static file handler and SPA fallback do not set a CSP header. Without CSP, the application is more vulnerable to XSS if any user-supplied content is rendered.
- **Risk:** XSS attacks not mitigated by browser-side CSP enforcement.
- **Suggested fix:** Add CSP header: `default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' ws: wss:`. Preview iframes are safe — they're served same-origin via the `/preview/` and `/deploy-preview/` reverse proxy (see `src/api/preview.rs`), so `frame-src` inherits `'self'` from `default-src`. The proxy already overrides `X-Frame-Options` to `SAMEORIGIN` on preview responses and strips backend CSP/CORS headers. Include `ws: wss:` for the WebSocket HMR bridge used by preview proxying.
- **Found by:** Agent 10

### A9: [HIGH] Pipeline state machine transitions not validated
- **Module:** pipeline/executor
- **File:** `src/pipeline/executor.rs:112-127`
- **Description:** The executor transitions pipeline status from `pending` to `running` and to `success`/`failure` using raw SQL string updates. There is no `PipelineStatus` enum or `can_transition_to()` guard. The `ReleasePhase` state machine in `deployer/types.rs` is well-modeled, but pipelines use raw strings.
- **Risk:** Invalid state transitions under concurrent conditions (e.g., cancelled pipeline set back to running).
- **Suggested fix:** Define a `PipelineStatus` enum with `can_transition_to()` and validate transitions before UPDATE.
- **Found by:** Agent 6

### A10: [HIGH] Reconciler `transition_phase()` does not validate state machine
- **Module:** deployer/reconciler
- **File:** `src/deployer/reconciler.rs:707-730`
- **Description:** `transition_phase()` takes a raw `new_phase` string and updates DB without calling `ReleasePhase::can_transition_to()`, despite the method existing in `types.rs`.
- **Risk:** Invalid phase transitions if any reconciler logic bug occurs.
- **Suggested fix:** Parse current/new phase into `ReleasePhase` enums, call `can_transition_to()`, return error if invalid.
- **Found by:** Agent 6

### A11: [HIGH] `write_audit()` blocks the HTTP response
- **Module:** audit
- **File:** `src/audit.rs:15-24`
- **Description:** `write_audit()` is `await`ed inline in handlers. If the audit DB insert is slow or pool exhausted, it blocks the response. CLAUDE.md documents it as "fire-and-forget" but the implementation contradicts this.
- **Risk:** Response latency spikes when audit writes are slow; cascading failures if pool exhausted.
- **Suggested fix:** Wrap in `tokio::spawn()` or update documentation to reflect blocking behavior.
- **Found by:** Agent 10

### A12: [HIGH] `check_name()` accepts Unicode — homoglyph risk
- **Module:** validation
- **File:** `src/validation.rs:15-31`
- **Description:** `check_name()` uses `char::is_alphanumeric()` which is Unicode-aware. Characters like Cyrillic 'а' pass. Names are used for git repo paths and K8s namespace slugs.
- **Risk:** Homoglyph attacks (visually identical but different names), unexpected behavior in filesystem paths.
- **Suggested fix:** Use `c.is_ascii_alphanumeric()` for slugs used in filesystem paths and K8s names.
- **Found by:** Agents 4, 10

### A13: [HIGH] `check_length()` allows null bytes and control characters
- **Module:** validation
- **File:** `src/validation.rs:5-13`
- **Description:** `check_length()` does not reject null bytes or control characters. Affects `check_labels()`, `check_setup_commands()`, and `check_browser_config()` origins which only call `check_length()`.
- **Risk:** Null byte injection, control character manipulation in stored data.
- **Suggested fix:** Add null byte / control character rejection in `check_length()` or in each affected validator.
- **Found by:** Agent 10

### A14: [HIGH] Flags module — entire API missing audit logging
- **Module:** api/flags
- **File:** `src/api/flags.rs:192-582`
- **Description:** All 8 mutation handlers (create, update, delete, toggle, add_rule, delete_rule, set_override, delete_override) write to `flag_history` but not to the platform-wide `audit_log` table.
- **Risk:** Feature flag changes invisible in the platform audit trail.
- **Suggested fix:** Add `write_audit()` calls to all flag mutation handlers.
- **Found by:** Agent 2

### A15: [HIGH] Deployments module — 6 mutation handlers missing audit logging
- **Module:** api/deployments
- **File:** `src/api/deployments.rs:331-713`
- **Description:** `create_target`, `adjust_traffic`, `promote_release`, `rollback_release`, `pause_release`, `resume_release` all lack `write_audit()`.
- **Risk:** Deployment operations invisible in audit trail.
- **Suggested fix:** Add `write_audit()` to all deployment mutation handlers.
- **Found by:** Agent 2

### A16: [HIGH] Config `Debug` derive exposes sensitive fields
- **Module:** config
- **File:** `src/config.rs:4`
- **Description:** `Config` derives `Debug` and `Clone`. Debug-printing would expose `database_url` (with password), `minio_secret_key`, `smtp_password`, `admin_password`, `master_key`.
- **Risk:** Sensitive credentials in logs if Config is ever debug-printed.
- **Suggested fix:** Implement manual `Debug` that redacts sensitive fields.
- **Found by:** Agent 10

### A17: [HIGH] No pipeline-level timeout
- **Module:** pipeline/executor
- **File:** `src/pipeline/executor.rs`
- **Description:** Individual steps have a 15-minute timeout, but there is no overall pipeline-level timeout. A pipeline with many steps could run indefinitely.
- **Risk:** Resource exhaustion from long-running pipelines.
- **Suggested fix:** Add a configurable pipeline-level timeout and check elapsed time in the step loop.
- **Found by:** Agent 6

### A18: [HIGH] `Gateway` resource allowed in user deploy manifests
- **Module:** deployer/applier
- **File:** `src/deployer/applier.rs:51`
- **Description:** `Gateway` is in `ALLOWED_KINDS`. In some Gateway API implementations, creating a Gateway can bind to infrastructure-level listeners, potentially capturing traffic intended for other tenants.
- **Risk:** Cross-tenant traffic capture via Gateway resource creation.
- **Suggested fix:** Remove "Gateway" from `ALLOWED_KINDS`; only allow `HTTPRoute` (which references existing Gateways via parentRefs).
- **Found by:** Agent 6

### A19: [HIGH] Receive-pack collects entire push body into memory
- **Module:** git/smart_http
- **File:** `src/git/smart_http.rs:461-465`
- **Description:** `receive_pack` calls `body.collect().await.to_bytes()` reading the full request body into memory. Large pushes can exhaust server memory.
- **Risk:** OOM from large git pushes.
- **Suggested fix:** Stream the body to git's stdin incrementally rather than buffering the entire pack file.
- **Found by:** Agent 8

### A20: [HIGH] No LFS upload size limit
- **Module:** git/lfs
- **File:** `src/git/lfs.rs:135`
- **Description:** LFS batch handler generates presigned URLs without checking `obj.size`. A malicious client could claim a 1 TB upload.
- **Risk:** Storage abuse via LFS.
- **Suggested fix:** Add size validation before presigned URL generation (e.g., configurable max of 5 GB).
- **Found by:** Agent 8

### A21: [HIGH] Registry `get_blob` reads entire blob into memory
- **Module:** registry/blobs
- **File:** `src/registry/blobs.rs:80`
- **Description:** `state.minio.read(&blob.minio_path).await?.to_vec()` reads the full blob into memory. Container image layers can be hundreds of MB.
- **Risk:** OOM from large container image layer requests.
- **Suggested fix:** Use streaming reads via presigned URLs or `minio.reader()` with `Body::from_stream()`.
- **Found by:** Agent 8

### A22: [HIGH] Production `.unwrap()` calls in pipeline code
- **Module:** pipeline
- **Files:** `src/pipeline/definition.rs:395,785`, `src/pipeline/executor.rs:557`
- **Description:** Three `.unwrap()` calls in production code. Definition.rs:395 unwraps on `deploy_test` option, :785 unwraps on `last()`, executor.rs:557 unwraps on semaphore acquire.
- **Risk:** Panics in production under edge conditions.
- **Suggested fix:** Replace with `.expect("reason")` or proper error handling.
- **Found by:** Agents 6, 11

### A23: [HIGH] Passkey login loads ALL credentials from database
- **Module:** api/passkeys
- **File:** `src/api/passkeys.rs:345-358`
- **Description:** `complete_login()` fetches every active passkey credential for all users to perform discoverable authentication. As user count grows, this becomes a performance/DoS vector.
- **Risk:** Database performance degradation and potential DoS.
- **Suggested fix:** Implement server-side pre-filtering by credential ID from the request.
- **Found by:** Agent 3

### A24: [HIGH] `logout()` deletes ALL user sessions, not just the current one
- **Module:** api/users
- **File:** `src/api/users.rs:275-277`
- **Description:** The SQL deletes all sessions for user_id, not just the session that made the request. One logout logs out every session (web, API, mobile).
- **Risk:** Unexpected session termination across all devices.
- **Suggested fix:** Delete only the current session by matching on the session token hash.
- **Found by:** Agent 3

### A25: [HIGH] Webhook URLs logged in notify module
- **Module:** notify/webhook
- **File:** `src/notify/webhook.rs:63,70`
- **Description:** Webhook URLs logged in `tracing::info` and `tracing::warn`. CLAUDE.md explicitly states "never log webhook URLs (may contain tokens)".
- **Risk:** Token leakage via structured log aggregation.
- **Suggested fix:** Remove or redact URL to hostname-only in log statements.
- **Found by:** Agent 9

---

## Medium Findings (should address)

### A26: [MEDIUM] Missing role permission cache invalidation
`src/api/admin.rs:243-299` — `set_role_permissions()` does not invalidate permission cache. Users keep stale permissions for up to 300s.
**Fix:** Query `user_roles` for affected users and call `invalidate_permissions()` for each.

### A27: [MEDIUM] Passkey rename missing audit log
`src/api/passkeys.rs:254-276` — The rename mutation has no audit trail, unlike delete which does.
**Fix:** Add `write_audit()` with action "auth.passkey_rename".

### A28: [MEDIUM] Passkey login rate limit ineffective
`src/api/passkeys.rs:336-339` — Rate limit keyed on `challenge_id` (unique per ceremony), so the limit is never triggered.
**Fix:** Rate limit by IP address instead.

### A29: [MEDIUM] Service account deactivation missing session cleanup
`src/api/admin.rs:637-682` — Deletes api_tokens but not auth_sessions, unlike user deactivation.
**Fix:** Add `DELETE FROM auth_sessions WHERE user_id = $1`.

### A30: [MEDIUM] `list_projects` excludes RBAC-granted private projects
`src/api/projects.rs:487-550` — Only shows private projects where user is owner. Users with explicit project:read grants don't see them in list view.
**Fix:** Include subquery/JOIN on permission grants.

### A31: [MEDIUM] Auto-merge synthetic AuthUser bypasses token scopes
`src/api/merge_requests.rs:1400-1417` — `try_auto_merge` constructs AuthUser with `token_scopes: None` and `boundary_*_id: None`, bypassing scope restrictions.
**Fix:** Store/restore original token scopes alongside auto_merge_by.

### A32: [MEDIUM] `evaluate_flags` missing key count validation
`src/api/flags.rs:637-660` — `body.keys` Vec has no length limit. Could cause excessive DB queries.
**Fix:** Add `if body.keys.len() > 100 { return Err(ApiError::BadRequest(...)); }`.

### A33: [MEDIUM] Global commands readable by any authenticated user
`src/api/commands.rs:408-442` — No permission check for global commands, including `prompt_template` content.
**Fix:** Require admin for global commands, or verify caller access.

### A34: [MEDIUM] Missing input validation across newer modules
Multiple files — `TriggerRequest.git_ref` (pipelines), `image_ref` (deployments), `provider_type` (llm_providers), `auth_type`/`token` (onboarding), `description` (flags), `commit_sha` (deployments), `env_vars` HashMap (llm_providers, onboarding) all lack validation.
**Fix:** Apply `crate::validation::*` helpers to all user inputs.

### A35: [MEDIUM] MR comments/reviews missing audit logging
`src/api/merge_requests.rs:904-946,1349-1373` — MR comment creation and disable_auto_merge lack audit entries.
**Fix:** Add `write_audit()` calls.

### A36: [MEDIUM] Parquet rotation not atomic — potential data loss
`src/observe/parquet.rs:72-117` — Upload + delete not in a transaction. If upload silently fails before DELETE, data is lost.
**Fix:** Verify upload success before DELETE, or use two-phase approach.

### A37: [MEDIUM] Agent setup_commands may lack validation in pod path
`src/agent/claude_code/pod.rs:740-744` — User-supplied setup_commands joined with `&&` and executed via `sh -c`. No visible call to `check_setup_commands()` validation in the agent pod path.
**Fix:** Verify setup_commands are validated before passing to `build_init_containers`.

### A38: [MEDIUM] Agent container allows privilege escalation
`src/agent/claude_code/pod.rs:30-34` — `allow_privilege_escalation: true` for sudo/package installation.
**Fix:** Consider restricting to non-kaniko steps or adding `seccompProfile: RuntimeDefault`.

### A39: [MEDIUM] Gateway traffic weights not validated to sum to 100
`src/deployer/gateway.rs:20-62` — Builder function accepts separate weights without validation.
**Fix:** Add assertion/error if `stable_weight + canary_weight != 100`.

### A40: [MEDIUM] Soft-delete workspace does not cascade to projects
`src/workspace/service.rs:174-182` — Projects under a soft-deleted workspace remain active and accessible.
**Fix:** Also soft-delete workspace projects, or prevent deletion if active projects exist.

### A41: [MEDIUM] `update_validation_status()` lacks user ownership check
`src/secrets/llm_providers.rs:319-337` — Updates any config by ID without verifying ownership.
**Fix:** Add `user_id` parameter and WHERE clause.

### A42: [MEDIUM] Valkey `KEYS` command used in production
`src/store/valkey.rs:52` — `invalidate_pattern()` uses KEYS which blocks Valkey for full key scan.
**Fix:** Replace with SCAN-based iteration.

### A43: [MEDIUM] ~80 async functions with side effects missing `#[tracing::instrument]`
Entire `api/flags.rs` (17 handlers), `api/deployments.rs` (18 handlers), `deployer/reconciler.rs` (27 functions), `deployer/analysis.rs` (9 functions) lack instrumentation.
**Fix:** Add `#[tracing::instrument(skip(state), err)]` to all async functions with DB/K8s side effects.

### A44: [MEDIUM] ~30 stale `#[allow(dead_code)]` annotations
`src/store/mod.rs`, `src/error.rs`, `src/config.rs`, `src/agent/claude_cli/mod.rs`, `src/notify/mod.rs`, `src/secrets/mod.rs`, `src/workspace/` — All modules fully implemented; allows are from incremental development.
**Fix:** Remove stale annotations, let compiler identify genuinely unused items.

### A45: [MEDIUM] `Response::builder().body().unwrap()` in 6 places
`src/api/pipelines.rs:378,383,408,412,418,508` — Technically infallible but violates no-unwrap rule.
**Fix:** Replace with `.expect("infallible: valid Response builder")` or `.map_err(ApiError::Internal)?`.

### A46: [MEDIUM] Release asset filename not sanitized
`src/api/releases.rs:386` — Multipart `file_name()` could contain path separators. Name placed directly in `content-disposition` header without escaping quotes/newlines.
**Fix:** Strip path separators and escape special characters per RFC 6266.

### A47: [MEDIUM] Stale delegation cache after expiry
`src/rbac/resolver.rs:32-35` — Expired delegations may remain active for up to 5 additional minutes (cache TTL).
**Fix:** For critical revocations, reduce TTL or add immediate invalidation for near-expiry delegations.

---

## Low Findings (optional)

- [LOW] A48: `src/api/projects.rs:620` — `update_project` returns 403 not 404 for private projects
- [LOW] A49: `src/api/issues.rs:350,564` — Revoked authors can still edit issues/comments (no project re-check)
- [LOW] A50: `src/api/merge_requests.rs:773` — Any reader can submit review approvals
- [LOW] A51: `src/api/merge_requests.rs:949` — MR comment edit no project-read check for revoked author
- [LOW] A52: `src/api/workspaces.rs:128` — `require_workspace_admin` returns 403 to non-members
- [LOW] A53: `src/api/projects.rs:494` — ILIKE search doesn't escape `%` / `_` metacharacters
- [LOW] A54: `src/api/webhooks.rs:183,329` — Webhook secret field no length validation
- [LOW] A55: `src/api/releases.rs:101` — `list_releases` no pagination
- [LOW] A56: `src/api/issues.rs:430`, `merge_requests.rs:724,856` — Comments/reviews no pagination
- [LOW] A57: `src/api/workspaces.rs:294` — `list_members` no pagination
- [LOW] A58: `src/api/webhooks.rs:501,473` — Webhook dispatch logs URLs (same as A25 but in api/webhooks)
- [LOW] A59: `src/api/admin.rs:154` — `parse_user_type()` maps parse errors to 500 instead of 400
- [LOW] A60: `src/api/admin.rs:483` — `create_service_account` ~105 lines (exceeds 100-line clippy limit)
- [LOW] A61: `src/api/passkeys.rs:330` — `complete_login` ~104 lines
- [LOW] A62: `src/api/passkeys.rs:469` — Passkey audit entry has `ip_addr: None`
- [LOW] A63: `src/api/users.rs:567` — `deactivate_user` returns 200 even if user doesn't exist
- [LOW] A64: `src/auth/rate_limit.rs:19` — TOCTOU race between INCR and EXPIRE
- [LOW] A65: `src/api/users.rs:148` — Rate limit key is username-only (no IP-based secondary limit)
- [LOW] A66: `src/validation.rs:44` — `check_url()` doesn't parse URL (accepts `http://` with no host)
- [LOW] A67: `src/validation.rs:54` — `check_branch_name()` doesn't block git-unsafe chars (`~`, `^`, `:`, `*`, `[`)
- [LOW] A68: `src/validation.rs:231` — `match_glob_pattern()` only handles single-wildcard patterns
- [LOW] A69: `src/agent/pubsub_bridge.rs:116` — Lagged broadcast messages silently dropped
- [LOW] A70: `src/agent/claude_code/pod.rs:878` — Chromium `--no-sandbox` in browser sidecar
- [LOW] A71: `src/agent/create_app.rs:496` — Prompt truncation at byte boundary may split UTF-8
- [LOW] A72: `src/pipeline/definition.rs:317` — No path traversal validation on artifact paths
- [LOW] A73: `src/pipeline/trigger.rs:520` — `read_file_at_ref` doesn't validate git_ref doesn't start with `-`
- [LOW] A74: `src/deployer/reconciler.rs:129` — Spawns tokio tasks without join/limit
- [LOW] A75: `src/deployer/ops_repo.rs:20` — Ops repo name passed to filesystem path without traversal check
- [LOW] A76: `src/deployer/namespace.rs:219` — `slugify_namespace()` returns empty string for all-special-char input
- [LOW] A77: `src/deployer/analysis.rs:309` — `invert_condition()` doesn't handle "eq" condition
- [LOW] A78: `src/deployer/analysis.rs:273` — `count_project_requests()` casts negative f64 to u64 (wraps)
- [LOW] A79: `src/observe/parquet.rs:74` — `fetch_all` with LIMIT 10000 loads all rows into memory
- [LOW] A80: `src/observe/query.rs:345` — ILIKE search doesn't escape `%` / `_` (same pattern as A53)
- [LOW] A81: `src/git/ssh_server.rs:360` — SSH push always falls back to default_branch
- [LOW] A82: `src/git/protection.rs:97` — `is_force_push` returns false (fail-open) when git fails
- [LOW] A83: `src/registry/blobs.rs:262` — `complete_upload` reassembles all chunks in memory
- [LOW] A84: `src/registry/manifests.rs:100` — Media type from user Content-Type stored without validation
- [LOW] A85: `src/secrets/engine.rs:38` — No `zeroize` on decrypted secrets in memory
- [LOW] A86: `src/secrets/request.rs` — No GC of timed-out secret requests
- [LOW] A87: `src/notify/email.rs:47` — SMTP password defaults to empty string silently
- [LOW] A88: `src/workspace/service.rs:101` — `create_workspace` not wrapped in transaction
- [LOW] A89: `src/onboarding/claude_auth.rs:170` — Auth code prefix (6 chars) logged
- [LOW] A90: `src/health/checks.rs:148` — Health check leaks filesystem path in response
- [LOW] A91: `src/main.rs:333` — Session cleanup task has no shutdown signal
- [LOW] A92: `src/main.rs:308` — Background task JoinHandles discarded at startup
- [LOW] A93: `src/store/eventbus.rs:163` — Eventbus spawns unbounded tasks without backpressure
- [LOW] A94: `src/error.rs:41,50` — `NotFound`/`BadRequest` msg could reflect user input (audit call sites)

---

## Module Health Summary

### api/ — NEEDS ATTENTION
Strong auth patterns in core CRUD (projects, issues, MRs), but newer modules (flags, deployments, onboarding, dashboard) have significant gaps: missing authorization checks, missing audit logging, missing input validation, and 403-instead-of-404 pattern violations. The flags module is the weakest — no audit log, no tracing instrumentation.

### agent/ — GOOD
Well-designed session lifecycle with proper cleanup, environment isolation, and RBAC. Key concern is `allow_privilege_escalation: true` on containers and potential setup_commands injection. The claude_cli submodule has stale dead_code annotations but is functionally sound.

### auth/ + rbac/ — GOOD
Timing-safe password verification, proper token hashing, workspace-derived permissions. The delegation chain escalation (A6) is the primary concern. Rate limiting is present but has minor gaps (passkey rate limit ineffective, no IP-based secondary limit).

### pipeline/ + deployer/ — NEEDS ATTENTION
Pipeline definition lacks container image validation (A7) and state machine enforcement (A9). Deployer reconciler doesn't validate phase transitions despite having the enum (A10). Gateway resource in ALLOWED_KINDS is a privilege escalation concern. No pipeline-level timeout.

### git/ + registry/ — NEEDS ATTENTION
SSH push bypasses branch protection (A3) is the most critical finding in the audit. Git smart HTTP buffers entire push bodies in memory (A19). Registry `get_blob` has the same issue (A21). No LFS upload size limit (A20). These are memory exhaustion and security bypass risks.

### observe/ + store/ — GOOD
Bounded channels, proper shutdown handling, rate-limited ingest, RBAC on queries. Minor gap in Parquet rotation atomicity (A36) and Valkey KEYS command (A42).

### secrets/ + notify/ + workspace/ — GOOD
AES-256-GCM encryption with proper nonce generation. Minor gaps: no zeroize, no key rotation, webhook URL logging violation. Workspace soft-delete doesn't cascade to projects.

### foundation — GOOD
Security headers present, body size limits configured, graceful shutdown, proper signal handling. Missing CSP header (A8) is the main gap. Config Debug derive is a latent risk (A16).

---

## Recommended Action Plan

### Immediate (this week)
1. **A1** — Add `require_admin()` to dashboard stats and audit log (2 lines each)
2. **A2** — Add session ownership check to onboarding auth endpoints (5 lines)
3. **A3** — Add branch protection enforcement to SSH push path (complex — consider pre-receive hook)
4. **A4** — Change 403 → 404 in 4 permission helpers (4 one-line changes)
5. **A5** — Add validation to admin role/delegation creation (3 one-line adds)
6. **A7** — Add `check_container_image()` to pipeline YAML validation (1 line)
7. **A25** — Remove webhook URLs from log statements (2 lines)

### Short-term (this month)
8. **A6** — Prevent delegation chain escalation (design decision needed)
9. **A8** — Add CSP header to main.rs or ui.rs
10. **A9, A10** — Enforce state machine transitions for pipelines and releases
11. **A11** — Make `write_audit()` non-blocking
12. **A12, A13** — Tighten validation: ASCII-only names, reject control chars in check_length
13. **A14, A15** — Add audit logging to flags and deployments modules
14. **A16** — Manual Debug impl for Config
15. **A17** — Add pipeline-level timeout
16. **A18** — Remove Gateway from ALLOWED_KINDS
17. **A19, A20, A21** — Stream large payloads (git push, LFS, registry blobs)

### Long-term (backlog)
18. **A43** — Add tracing instrumentation to ~80 uninstrumented functions
19. **A44** — Remove ~30 stale dead_code annotations
20. **A34** — Systematic input validation pass across all newer modules
21. Implement `zeroize` for sensitive memory (A85)
22. Key rotation mechanism for `PLATFORM_MASTER_KEY`
23. Replace Valkey KEYS with SCAN (A42)
24. Pagination for unpaginated list endpoints (A55-57)
