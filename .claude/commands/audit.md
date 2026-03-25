# Skill: Full Codebase Audit — Deep Review of Entire `src/`

**Description:** Orchestrates 10+ parallel AI agents that perform a comprehensive, module-by-module audit of the entire `src/` directory (~72K LOC, 113 files, 16 modules). Each agent reads every file in its assigned scope, applies domain-specific checklists, and produces categorized findings. A final synthesis merges all findings into a prioritized audit report.

**When to use:** Periodic health check, before a major release, after significant refactoring, or when onboarding to the codebase. Unlike the `review` skill (which reviews diffs/changes), this reviews **everything**.

---

## Orchestrator Instructions

You are the **Lead Auditor Agent**. Your job is to:

1. Verify the codebase compiles and lints cleanly (fast pre-check)
2. Launch parallel audit agents that each read every file in their assigned scope
3. Collect and synthesize findings into a comprehensive audit report
4. Produce a persistent `plans/codebase-audit-<date>.md` report

### Severity Levels

Every finding MUST have a severity:

| Severity | Meaning | Action |
|---|---|---|
| **CRITICAL** | Security vulnerability, data loss risk, crash in production | Must fix immediately |
| **HIGH** | Logic bug, missing auth/validation, broken invariant, unsound code | Should fix soon |
| **MEDIUM** | Code smell, inconsistent pattern, missing observability, tech debt | Fix when touching the file |
| **LOW** | Style nit, minor naming, optional improvement | Fix only if trivial |
| **INFO** | Observation, good pattern worth noting, architecture insight | No action needed |

---

## Phase 0: Pre-flight Check

Before launching agents, run quick checks to establish baseline:

```bash
just fmt -- --check      # formatting clean?
just lint                # clippy clean?
just deny                # dependency audit clean?
just test-unit           # unit tests pass?
```

If any of these fail, note the failures — agents will find the root causes. Don't block the audit on pre-flight failures.

Also run:
```bash
# Get file counts and LOC per module
find src -name '*.rs' | sed 's|/[^/]*$||' | sort | uniq -c | sort -rn
wc -l src/**/*.rs src/*.rs 2>/dev/null | sort -rn | head -30
```

This gives agents context on module sizes and helps prioritize.

---

## Phase 1: Parallel Module Audits

Launch **all 10 agents concurrently** using the Agent tool with `subagent_type: "general-purpose"`. Each agent gets a specific module scope and checklist.

**Critical instructions for EVERY agent prompt:**
- List the exact files the agent must read (use the file listing from Phase 0)
- Tell the agent to READ every file completely — no skimming, no assumptions
- Include the relevant CLAUDE.md sections as context
- Require output in the structured format: `[SEVERITY] file:line — description\n  Fix: ...`
- Set max_turns high enough for thorough reading (20-30 turns)
- Tell the agent it is performing an AUDIT (read-only) — it must NOT edit any files

---

### Agent 1: API Handlers — Core CRUD (auth, validation, patterns)

**Scope:** `src/api/projects.rs`, `src/api/issues.rs`, `src/api/merge_requests.rs`, `src/api/webhooks.rs`, `src/api/workspaces.rs`, `src/api/releases.rs`, `src/api/mod.rs`, `src/api/helpers.rs`

**Read ALL files listed above, then check:**

_Authentication & Authorization:_
- [ ] Every handler takes `auth: AuthUser` (or documents why not)
- [ ] Read endpoints call `require_project_read()` or equivalent
- [ ] Write endpoints call `require_project_write()` or equivalent
- [ ] Private resources return **404** (not 403) — no existence leakage
- [ ] Token scope enforcement: `auth.check_project_scope()` / `auth.check_workspace_scope()` where applicable
- [ ] No IDOR vulnerabilities — verify resource belongs to authenticated user's accessible scope

_Input Validation:_
- [ ] Every string field from user input has length validation via `crate::validation::*`
- [ ] Names/slugs: 1-255, Emails: 3-254, Titles: 1-500, Bodies: 0-100,000, URLs: 1-2048
- [ ] Labels: max 50 items, each 1-100 chars
- [ ] No raw SQL string concatenation — all queries parameterized
- [ ] JSON parsing uses typed deserialization (`Json<T>`) — no raw `serde_json::Value` for input

_Audit Logging:_
- [ ] Every mutation writes to `audit_log` via `AuditEntry`
- [ ] Audit entries include: actor_id, actor_name, action, resource, resource_id, ip_addr
- [ ] Audit detail never contains secrets, tokens, or URLs

_Handler Patterns:_
- [ ] Signature follows: `State(state), auth: AuthUser, Path(..), Query(..), Json(..)`
- [ ] No handler exceeds 100 lines (clippy `too_many_lines`)
- [ ] No function exceeds 7 parameters (clippy `too_many_arguments`)
- [ ] Pagination uses `ListParams` with limit (default 50, max 100) / offset (default 0)
- [ ] List responses use `ListResponse<T> { items, total }`
- [ ] Soft-delete respected: `AND is_active = true` on project queries

_Webhook dispatch:_
- [ ] Mutations that external systems care about call `fire_webhooks()`
- [ ] Webhook events use correct event names (push, mr, issue, build, deploy)

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 2: API Handlers — Platform Services

**Scope:** `src/api/pipelines.rs`, `src/api/deployments.rs`, `src/api/sessions.rs`, `src/api/secrets.rs`, `src/api/notifications.rs`, `src/api/commands.rs`, `src/api/onboarding.rs`, `src/api/dashboard.rs`, `src/api/downloads.rs`, `src/api/flags.rs`, `src/api/llm_providers.rs`, `src/api/preview.rs`

**Apply the same checklist as Agent 1** (auth, validation, audit, patterns), plus:

_Secrets handling:_
- [ ] Secret values never appear in API responses (only metadata)
- [ ] Secret values never logged
- [ ] Encryption uses `secrets::engine` — no plaintext storage
- [ ] Master key access is properly scoped

_Pipeline/Deploy endpoints:_
- [ ] Pipeline triggers validate `.platform.yaml` before execution
- [ ] Deployment endpoints check appropriate permissions
- [ ] No unbounded list queries — pagination enforced

_Session management:_
- [ ] Agent session endpoints check session ownership
- [ ] Session cleanup on termination
- [ ] No session fixation vulnerabilities

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 3: API Handlers — Auth & Admin

**Scope:** `src/api/admin.rs`, `src/api/users.rs`, `src/api/passkeys.rs`, `src/api/ssh_keys.rs`, `src/api/gpg_keys.rs`, `src/api/user_keys.rs`, `src/api/cli_auth.rs`, `src/api/setup.rs`, `src/api/health.rs`, `src/api/branch_protection.rs`

**Apply the same checklist as Agent 1**, plus:

_Admin endpoints:_
- [ ] All admin endpoints check `Permission::Admin*` or use `require_admin()`
- [ ] User creation properly hashes passwords with argon2
- [ ] User deactivation deletes sessions + tokens + invalidates permission cache
- [ ] Role/delegation changes invalidate permission cache

_Key management:_
- [ ] SSH key validation prevents injection
- [ ] GPG key parsing is safe against malformed input
- [ ] Key deletion cascades properly

_Setup/health:_
- [ ] Setup endpoint is protected against re-running after initial setup
- [ ] Health endpoint doesn't leak sensitive system information

_CLI auth:_
- [ ] CLI auth flow is resistant to CSRF
- [ ] Token exchange validates all parameters
- [ ] Rate limiting on auth endpoints

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 4: Auth, RBAC & Security Core

**Scope:** `src/auth/middleware.rs`, `src/auth/password.rs`, `src/auth/token.rs`, `src/auth/rate_limit.rs`, `src/auth/passkey.rs`, `src/auth/user_type.rs`, `src/auth/cli_creds.rs`, `src/auth/mod.rs`, `src/rbac/resolver.rs`, `src/rbac/types.rs`, `src/rbac/delegation.rs`, `src/rbac/mod.rs`, `src/validation.rs`

**Read ALL files, then check:**

_Password handling:_
- [ ] Argon2 used for hashing (not bcrypt, scrypt, or SHA)
- [ ] Timing-safe comparison: always run argon2 verify, use `dummy_hash()` for missing users
- [ ] Salt generated with `argon2::password_hash::rand_core::OsRng` (NOT `rand::rng()`)
- [ ] Password complexity enforced at handler boundary
- [ ] No plaintext passwords in logs, errors, or audit entries

_Token handling:_
- [ ] Tokens hashed with SHA-256 before DB storage
- [ ] Token comparison is timing-safe (hash comparison, not plaintext)
- [ ] Token expiry enforced (1-365 days, default 90)
- [ ] Revoked/expired tokens rejected immediately
- [ ] Token prefix (`plat_`) doesn't leak security-relevant info

_Session handling:_
- [ ] Session cookies have `HttpOnly`, `SameSite=Strict`, `Secure` (when configured)
- [ ] Session expiry enforced
- [ ] Session cleanup runs periodically

_RBAC resolution:_
- [ ] Permission cache uses proper scoping (user_id + project_id)
- [ ] Cache invalidation on role/delegation changes
- [ ] Workspace-derived permissions correctly computed
- [ ] No permission escalation through delegation chains
- [ ] `has_permission_scoped()` enforces token scope boundaries
- [ ] Boundary fields (boundary_workspace_id, boundary_project_id) properly restrict access

_Rate limiting:_
- [ ] Login endpoint has rate limiting
- [ ] Rate limit keys include client identifier (not just global)
- [ ] Rate limit window and threshold are reasonable
- [ ] Rate limit bypass not possible via header manipulation

_Validation helpers:_
- [ ] `check_name()` rejects null bytes, control characters
- [ ] `check_email()` has reasonable validation
- [ ] `check_url()` only allows http(s)
- [ ] `check_branch_name()` rejects `..` (path traversal)
- [ ] `check_container_image()` prevents shell injection
- [ ] `check_lfs_oid()` is exactly 64 hex chars
- [ ] No regex denial-of-service (ReDoS) in validation patterns

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 5: Agent Module (Claude CLI, sessions, identity)

**Scope:** All files under `src/agent/` (23 files)

**Read ALL files, then check:**

_Session lifecycle:_
- [ ] Sessions have proper creation → running → terminated state machine
- [ ] Session cleanup removes all resources (pods, tokens, Valkey keys)
- [ ] Orphaned sessions detected and cleaned up
- [ ] Session timeout enforced

_Ephemeral identity:_
- [ ] Agent identities have minimal required permissions
- [ ] Agent tokens are scoped to specific project
- [ ] Identity cleanup on session termination
- [ ] No privilege escalation from agent identity

_Claude CLI integration:_
- [ ] CLI invocation sanitizes all arguments (no command injection)
- [ ] CLI output parsing handles malformed/unexpected responses
- [ ] CLI process has resource limits (CPU, memory, timeout)
- [ ] CLI errors don't leak sensitive configuration

_Provider configuration:_
- [ ] `resolve_image()` validates image names
- [ ] Registry URLs properly constructed (no injection)
- [ ] Default images are pinned to specific versions/digests

_Pub/sub bridge:_
- [ ] Message serialization/deserialization is safe
- [ ] Channel naming prevents cross-session leakage
- [ ] Message size limits enforced
- [ ] Proper error handling on connection loss

_Valkey ACL:_
- [ ] ACL rules follow principle of least privilege
- [ ] `resetkeys resetchannels -@all` baseline applied
- [ ] `+ping` included for keepalive
- [ ] ACL cleanup on session termination

_K8s pod management:_
- [ ] Pods have resource limits (CPU, memory)
- [ ] Pods use per-project namespaces (`{slug}-dev`)
- [ ] Pod cleanup on session termination
- [ ] SecurityContext properly configured (no privileged containers)
- [ ] Service account tokens scoped appropriately

_Error handling:_
- [ ] All error paths handled (no `.unwrap()` in production)
- [ ] Error types use `thiserror`
- [ ] Errors mapped to appropriate API responses
- [ ] Sensitive details stripped from error messages

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 6: Pipeline & Deployer (K8s orchestration)

**Scope:** All files under `src/pipeline/` (5 files) and `src/deployer/` (11 files)

**Read ALL files, then check:**

_Pipeline definition:_
- [ ] `.platform.yaml` parsing validates all fields
- [ ] `check_container_image()` applied to user-supplied images
- [ ] `check_setup_commands()` validates commands against injection
- [ ] Step types validated against known set
- [ ] No path traversal in artifact paths

_Pipeline executor:_
- [ ] State machine: Pending → Running → Success/Failure/Cancelled — enforced via `can_transition_to()`
- [ ] K8s pods have resource limits
- [ ] Pod cleanup on failure/cancellation
- [ ] Executor loop uses `pipeline_notify` (not polling)
- [ ] Timeout enforcement on running pipelines
- [ ] Log collection handles pod crashes gracefully
- [ ] No unbounded retries

_Pipeline trigger:_
- [ ] Trigger validation prevents unauthorized builds
- [ ] Branch filtering works correctly
- [ ] No duplicate triggers for same commit

_Deployer reconciler:_
- [ ] Reconciliation loop has proper error recovery
- [ ] Drift detection is accurate
- [ ] No infinite reconciliation loops
- [ ] Graceful degradation when K8s API is unavailable

_Deployer applier:_
- [ ] `kind_to_plural()` map covers all used resource types
- [ ] Server-side apply uses correct field manager
- [ ] Resource cleanup for removed manifests
- [ ] Proper conflict resolution strategy

_Deployer renderer:_
- [ ] Kustomize overlay rendering is safe (no injection via template values)
- [ ] Rendered manifests validated before apply
- [ ] No path traversal in overlay paths

_Deployer gateway:_
- [ ] Traffic splitting percentages validated (sum to 100)
- [ ] Gateway configuration prevents misrouting
- [ ] TLS configuration is secure

_Preview environments:_
- [ ] `slugify_branch()` produces valid K8s names
- [ ] TTL cleanup runs reliably
- [ ] Namespace isolation between previews
- [ ] Preview cleanup removes ALL resources (deployments, services, ingresses, etc.)

_Ops repo:_
- [ ] Git operations are safe (no injection via branch/commit names)
- [ ] Repo storage path validated (no traversal)
- [ ] Concurrent access to same repo handled

_Namespace management:_
- [ ] Namespace creation validates names
- [ ] Namespace deletion cascades properly
- [ ] No cross-namespace access

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 7: Observe & Store (telemetry, data, bootstrap)

**Scope:** All files under `src/observe/` (10 files) and `src/store/` (6 files)

**Read ALL files, then check:**

_OTLP ingest:_
- [ ] Protobuf deserialization handles malformed data gracefully
- [ ] No unbounded memory allocation from large payloads
- [ ] Input size limits enforced
- [ ] Proper error responses for malformed OTLP data

_Parquet storage:_
- [ ] Time-based rotation is reliable (no data loss during rotation)
- [ ] Parquet files properly closed/flushed
- [ ] MinIO upload handles network failures (retries, idempotency)
- [ ] Disk usage bounded (rotation prevents unbounded growth)
- [ ] File naming avoids collisions

_Query engine:_
- [ ] Time-range filters properly applied
- [ ] No SQL injection via query parameters
- [ ] Result sets bounded (pagination/limits)
- [ ] Query timeout enforcement
- [ ] No full-table scans on large datasets

_Alert evaluation:_
- [ ] Alert rules validated before evaluation
- [ ] No unbounded alert storms (dedup/throttling)
- [ ] Alert notification delivery is resilient
- [ ] Alert state properly tracked (firing → resolved)

_Correlation:_
- [ ] Trace/span correlation is accurate
- [ ] Cross-service correlation handles missing spans

_Background tasks:_
- [ ] All 5 background tasks have proper shutdown handling
- [ ] Tasks don't panic on transient errors
- [ ] Tasks have proper logging/tracing
- [ ] Resource cleanup on shutdown

_Store bootstrap:_
- [ ] Bootstrap is idempotent (safe to run multiple times)
- [ ] Default admin credentials only in dev mode (`PLATFORM_DEV=true`)
- [ ] Migration ordering is correct
- [ ] Connection pool settings are reasonable

_Eventbus:_
- [ ] Pub/sub messages properly serialized
- [ ] Channel naming avoids collisions
- [ ] Error handling on publish/subscribe failures
- [ ] No message loss guarantee documented/handled

_Valkey connection:_
- [ ] Connection pool sizing is reasonable
- [ ] Reconnection handled gracefully
- [ ] No connection leaks

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 8: Git & Registry (protocol, storage)

**Scope:** All files under `src/git/` (12 files) and `src/registry/` (11 files)

**Read ALL files, then check:**

_Git smart HTTP:_
- [ ] `git-upload-pack` / `git-receive-pack` properly authenticated
- [ ] Push authorization checks project write permission
- [ ] No command injection via repository names or branch names
- [ ] Large pack file handling (memory limits, streaming)
- [ ] Proper error responses for git protocol errors

_Git hooks:_
- [ ] Server-side hooks validate push content
- [ ] No bypass via special branch names or ref patterns
- [ ] Hook execution has timeout
- [ ] Hook failures properly reported to client

_Git LFS:_
- [ ] LFS OID validation (64 hex chars)
- [ ] LFS upload size limits enforced
- [ ] MinIO storage paths validated (no traversal)
- [ ] Batch API properly paginated

_Git SSH server:_
- [ ] SSH key authentication is secure
- [ ] Key lookup timing-safe (no user enumeration)
- [ ] Command parsing prevents injection
- [ ] Session resource limits
- [ ] `russh` + `rand` boundary respected (no shared RNG)

_Git browser:_
- [ ] Path traversal prevented in file browsing
- [ ] Symlink following controlled
- [ ] Binary file handling (no unbounded memory allocation)
- [ ] Ref resolution handles edge cases (deleted branches, tags)

_Branch protection:_
- [ ] Protection rules enforced on push
- [ ] Admin bypass properly authorized
- [ ] Pattern matching is safe (no ReDoS)

_Git signature:_
- [ ] Signature verification is correct
- [ ] No signature bypass

_Git templates:_
- [ ] Template rendering prevents injection
- [ ] Default branch configuration is safe

_Registry auth:_
- [ ] Token-based auth for Docker registry protocol
- [ ] Scope validation on token claims
- [ ] Pull vs push authorization differentiated

_Registry blobs:_
- [ ] Upload size limits enforced
- [ ] Digest verification (content matches claimed digest)
- [ ] No path traversal in blob storage
- [ ] Chunked upload handling is correct
- [ ] Blob cleanup (GC) for unreferenced blobs

_Registry manifests:_
- [ ] Manifest validation (schema, media type)
- [ ] Content-addressable storage correct
- [ ] Manifest list/index handling
- [ ] Cross-repository mounting authorization

_Registry GC:_
- [ ] GC doesn't delete referenced blobs
- [ ] GC handles concurrent uploads safely
- [ ] GC has proper locking/coordination

_Registry pull secrets:_
- [ ] Pull secrets properly scoped
- [ ] Secret rotation handled
- [ ] No plaintext storage of pull secrets

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 9: Secrets, Notify, Workspace, Onboarding, Health

**Scope:** All files under `src/secrets/` (4 files), `src/notify/` (3 files), `src/workspace/` (3 files), `src/onboarding/` (4 files), `src/health/` (3 files)

**Read ALL files, then check:**

_Secrets engine:_
- [ ] AES-256-GCM encryption with proper nonce generation (never reused)
- [ ] Master key loaded securely (env var, not hardcoded)
- [ ] Encrypt-at-rest, decrypt-on-read pattern enforced
- [ ] No plaintext secret values in logs, errors, or DB
- [ ] Key rotation story (or documented gap)
- [ ] Proper zeroing of sensitive memory (zeroize crate or equivalent)

_LLM providers:_
- [ ] API keys encrypted at rest
- [ ] Provider validation (supported providers only)
- [ ] No API key leakage in error messages

_Secret request flow:_
- [ ] Request approval flow is secure
- [ ] No bypass of approval chain
- [ ] Proper audit trail

_User keys:_
- [ ] Key material properly validated
- [ ] No key material in logs

_Notify dispatch:_
- [ ] Notification routing is correct
- [ ] No PII leakage in notification content
- [ ] Notification preferences respected

_Email:_
- [ ] SMTP credentials not logged
- [ ] Email content properly sanitized (no injection)
- [ ] TLS enforced for SMTP (STARTTLS or implicit)
- [ ] Recipient validation

_Notify webhook:_
- [ ] HMAC-SHA256 signing correct
- [ ] Webhook timeouts enforced (5s connect, 10s total)
- [ ] SSRF protection on webhook URLs
- [ ] Concurrent delivery limited (semaphore)
- [ ] Webhook URLs never logged

_Workspace:_
- [ ] Workspace CRUD has proper auth
- [ ] Workspace membership grants correct implicit permissions
- [ ] Workspace deletion cascades correctly
- [ ] Cross-workspace isolation enforced

_Onboarding:_
- [ ] Demo project creation is safe (no injection via project name)
- [ ] Template rendering validates inputs
- [ ] Onboarding is idempotent or guarded against re-run

_Health:_
- [ ] Health check doesn't leak sensitive info
- [ ] Dependencies checked (DB, Valkey, MinIO, K8s)
- [ ] Health check is fast (no long-running queries)

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 10: Foundation & Architecture (root files, cross-cutting)

**Scope:** `src/main.rs`, `src/lib.rs`, `src/config.rs`, `src/error.rs`, `src/audit.rs`, `src/ui.rs`, `src/validation.rs`

**Read ALL files, then check:**

_main.rs:_
- [ ] Security headers applied: `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy`
- [ ] Body size limits configured (10 MB default, 500 MB for Git/LFS)
- [ ] CORS configured via `PLATFORM_CORS_ORIGINS` (denied by default)
- [ ] Graceful shutdown handles in-flight requests
- [ ] Background task spawning is correct (all 5+ tasks started)
- [ ] Session cleanup task runs periodically
- [ ] No panic-inducing code in startup path
- [ ] Proper signal handling (SIGTERM, SIGINT)

_config.rs:_
- [ ] All env vars documented with defaults
- [ ] Sensitive env vars (MASTER_KEY, etc.) not logged at startup
- [ ] Config validation catches invalid combinations early
- [ ] Dev mode (`PLATFORM_DEV`) clearly gated — no dev features leak to production
- [ ] Default values are secure (e.g., `SECURE_COOKIES` default should be true in prod)

_error.rs:_
- [ ] `ApiError` maps to correct HTTP status codes
- [ ] Error responses don't leak internal details
- [ ] Error serialization is consistent (JSON format)
- [ ] No catch-all `Internal(String)` for known failure modes

_audit.rs:_
- [ ] Audit log insertion is fire-and-forget (doesn't block the response)
- [ ] Audit log schema covers all needed fields
- [ ] No sensitive data in audit entries

_ui.rs:_
- [ ] Static files served with proper cache headers
- [ ] SPA fallback routing is correct
- [ ] No path traversal in static file serving
- [ ] Content-Security-Policy headers appropriate

_lib.rs:_
- [ ] Module re-exports are clean
- [ ] No `#[allow(dead_code)]` on items that should be removed
- [ ] Feature flags (if any) are documented

**Output:** Numbered findings with severity, file:line, description, and fix.

---

## Phase 2: Cross-Cutting Analysis

After Phase 1 agents return, launch **2 additional agents** that scan the ENTIRE `src/` for patterns that span modules.

### Agent 11: Error Handling & Observability Consistency

**Scope:** Entire `src/` directory

**Scan all .rs files for:**

_Error handling consistency:_
- [ ] All `.unwrap()` calls in production code (should be zero — list every one found)
- [ ] All `.expect()` calls — verify each is truly infallible with a descriptive message
- [ ] Inconsistent error types (some modules using `anyhow`, others `thiserror`)
- [ ] Missing `From` impls (error propagation requires explicit `.map_err()`)
- [ ] `todo!()` or `unimplemented!()` in production code

_Observability consistency:_
- [ ] Functions with DB/K8s/HTTP side effects missing `#[tracing::instrument]`
- [ ] `tracing::instrument` calls that don't `skip(pool, state, config)`
- [ ] String interpolation in log macros instead of structured fields
- [ ] Missing correlation context (user_id, project_id, session_id)
- [ ] Sensitive data in log statements (search for: password, token, secret, key, credential)

_Dead code & unused imports:_
- [ ] `#[allow(dead_code)]` annotations that may no longer be needed
- [ ] `#[allow(unused_*)]` annotations — are they still justified?
- [ ] Public functions/types that are never used outside their module

**Output:** Numbered findings with severity, file:line, description.

---

### Agent 12: Dependency Safety & Resource Management

**Scope:** `Cargo.toml`, `deny.toml`, entire `src/` directory

**Check:**

_Dependency safety:_
- [ ] No `unsafe` code (enforced by `unsafe_code = "forbid"` — verify the lint is present)
- [ ] No `openssl` dependency (banned in `deny.toml` — verify)
- [ ] `Cargo.lock` committed (binary project)
- [ ] No deprecated crate versions with known CVEs

_Resource management:_
- [ ] All spawned tokio tasks are properly awaited or tracked for shutdown
- [ ] All K8s watchers/informers have proper cleanup
- [ ] Database connections returned to pool (no leaks from long-running operations)
- [ ] Valkey connections returned to pool
- [ ] File handles properly closed (especially in git operations)
- [ ] No unbounded channels or queues
- [ ] Temporary files cleaned up
- [ ] All `Arc<Mutex<T>>` or `Arc<RwLock<T>>` usage avoids deadlocks (lock ordering)

_Concurrency patterns:_
- [ ] No data races from shared mutable state without synchronization
- [ ] Semaphore usage correct (webhook dispatch, concurrent operations)
- [ ] `Notify` usage correct (pipeline executor wake-up)
- [ ] Channel capacity bounded where used
- [ ] No busy-wait loops

_Memory safety:_
- [ ] No unbounded `Vec` growth from user input
- [ ] No unbounded `String` concatenation from user input
- [ ] Streaming used for large payloads (git packs, blob uploads)
- [ ] Response bodies bounded

**Output:** Numbered findings with severity, file:line, description.

---

## Phase 3: Synthesis

Once all 12 agents return, synthesize into a single report.

### Synthesis rules

1. **Deduplicate** — if multiple agents flag the same issue, merge into one finding with the highest severity
2. **Prioritize** — CRITICAL and HIGH first, always. Don't bury security issues under style nits.
3. **Categorize** — group findings by:
   - Security vulnerabilities
   - Logic bugs & correctness issues
   - Missing validation / auth gaps
   - Observability gaps
   - Code quality & consistency
   - Resource management
   - Architecture concerns
4. **Be actionable** — every finding above LOW must have a concrete fix
5. **Credit good patterns** — include a "Strengths" section noting well-implemented patterns (5-10 items)
6. **Number every finding** — A1, A2, A3... (A for Audit, distinguishing from review R-prefix)
7. **Tally statistics** — total findings by severity, per module, top 3 riskiest modules

---

## Phase 4: Write Audit Report

Persist the report as `plans/codebase-audit-<YYYY-MM-DD>.md`.

### Report structure

```markdown
# Codebase Audit Report

**Date:** <today>
**Scope:** Full `src/` directory — <N> files, ~<N>K LOC, <N> modules
**Auditor:** Claude Code (automated)
**Pre-flight:** fmt ✓/✗ | lint ✓/✗ | deny ✓/✗ | unit tests ✓/✗

## Executive Summary
- Overall health: GOOD / NEEDS ATTENTION / CRITICAL ISSUES
- {2-3 sentences on overall quality}
- Findings: X critical, Y high, Z medium, W low
- Top risks: {1-3 bullet points}
- Strengths: {1-3 bullet points}

## Statistics

| Module | Files | LOC | Critical | High | Medium | Low |
|---|---|---|---|---|---|---|
| api/ | N | N | N | N | N | N |
| ... | ... | ... | ... | ... | ... | ... |
| **Total** | **N** | **~NK** | **N** | **N** | **N** | **N** |

## Strengths
- {Good pattern 1 — where it's used, why it's good}
- {Good pattern 2}
- ...

## Critical & High Findings (must address)

### A1: [CRITICAL] {title}
- **Module:** {module}
- **File:** `src/path/file.rs:42`
- **Description:** {what's wrong}
- **Risk:** {what could happen in production}
- **Suggested fix:** {specific code change or approach}
- **Found by:** Agent {N} ({agent name})

### A2: [HIGH] {title}
...

## Medium Findings (should address)

### AN: [MEDIUM] {title}
- **Module:** {module}
- **File:** `src/path/file.rs:42`
- **Description:** {what's wrong}
- **Suggested fix:** {specific approach}

## Low Findings (optional)

- [LOW] A{N}: `src/path/file.rs:10` — {one-line description} → {one-line fix}

## Module Health Summary

### api/ — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences summarizing auth coverage, validation, patterns}

### agent/ — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences}

### auth/ + rbac/ — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences}

... (one section per module group)

## Recommended Action Plan

### Immediate (this week)
1. {Fix critical/high finding A1 — estimated scope}
2. ...

### Short-term (this month)
1. {Fix medium findings in {module}}
2. ...

### Long-term (backlog)
1. {Architecture improvements}
2. ...
```

### Rules
- Every finding gets a unique ID (A1, A2, ...) — allows referencing in follow-up work
- The report must be self-contained — readable without conversation context
- Do NOT include INFO-level items in the report (keep it actionable)
- Include the statistics table even if all categories show zero findings
- List strengths before weaknesses — balanced assessment, not just a bug list

---

## Phase 5: Summary to User

After writing the report, provide a concise summary:

1. Overall health assessment (one sentence)
2. Finding counts by severity
3. Top 3 most critical findings (one line each)
4. Top 3 strengths (one line each)
5. Path to the full report file
6. Suggested next steps (e.g., "Run `/dev` to fix A1-A3, then re-audit")

---

## Usage Notes

- This skill audits the **entire codebase**, not just changes. For change-based review, use `/review`.
- Expect 15-25 minutes for the full audit (12 parallel agents reading ~72K LOC).
- The audit is **read-only** — no files are modified. To fix findings, use the `/dev` skill.
- Audit reports are saved in `plans/` and can be referenced by the `/dev` and `/finalize` skills.
- If the codebase has grown significantly since the file listing in this skill, update the agent scope assignments.
- For focused audits of specific modules, tell the orchestrator which modules to audit — it can skip irrelevant agents.
