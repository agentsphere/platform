# Skill: Security Audit — Full Platform Attack Surface Review

**Description:** Orchestrates 10 parallel AI agents that perform a security-focused audit of the **entire platform** — Rust monolith, agent runner CLI, MCP servers, Web UI, Docker images, Helm chart, infrastructure scripts, and CI/CD. Unlike `/audit` (code quality of `src/`) and `/audit-ecosystem` (integration contracts), this skill evaluates the platform from an **attacker's perspective**: authentication bypass, privilege escalation, injection, SSRF, supply chain, secrets exposure, container escape, and network segmentation.

**When to use:** Before a security review, before opening the platform to external users, after changes to auth/RBAC/secrets/networking, or as a periodic security posture check. This is the skill to run when you want to know: *"Can someone break in, escalate, or exfiltrate?"*

---

## Orchestrator Instructions

You are the **Security Auditor**. Your job is to:

1. Run pre-flight security checks (tooling, configuration baseline)
2. Launch 10 parallel agents that each perform depth-first security analysis on a specific attack surface
3. Collect, deduplicate, and risk-score findings
4. Produce a persistent `plans/security-audit-<date>.md` report with CVSS-like severity and remediation

### Severity Levels (security-calibrated)

| Severity | Meaning | Examples | Action |
|---|---|---|---|
| **CRITICAL** | Exploitable without authentication, or leads to full platform compromise | RCE, auth bypass, unauthenticated admin access, master key exposure | Stop-ship. Fix immediately. |
| **HIGH** | Exploitable by authenticated user to escalate privileges or access unauthorized data | IDOR, privilege escalation, SSRF to internal services, secret exfiltration, SQL injection | Must fix before any external exposure |
| **MEDIUM** | Exploitable but requires specific conditions or has limited blast radius | Missing rate limit, information disclosure, insecure defaults, missing audit log | Fix within current sprint |
| **LOW** | Defense-in-depth improvement, hardening recommendation | Missing security header, verbose error message, optional encryption | Fix when convenient |
| **INFO** | Good security practice worth noting | Proper use of timing-safe comparison, correct RBAC pattern | No action needed |

---

## Phase 0: Security Pre-flight

Before launching agents, establish the security configuration baseline.

```bash
# 1. Check for known vulnerable dependencies
cargo audit 2>&1 || echo "cargo-audit not installed"
cd mcp && npm audit --production 2>&1 | tail -20; cd ..

# 2. Check for leaked secrets in codebase
which gitleaks && gitleaks detect --source . --no-git 2>&1 | tail -20 || echo "gitleaks not installed — skip"

# 3. Verify deny.toml bans
grep -A5 "deny" deny.toml | head -20

# 4. Verify unsafe is forbidden
grep "unsafe_code" Cargo.toml

# 5. List all public-facing entry points (routes, ports, protocols)
echo "=== Platform ports ==="
grep -r "PLATFORM_PORT\|PLATFORM_SSH_PORT\|listen\|bind" src/config.rs src/main.rs | head -20
echo "=== API routes ==="
grep -r "\.route\b\|\.nest\b" src/api/mod.rs | head -30
echo "=== Unauthenticated routes ==="
grep -r "without_auth\|no_auth\|public" src/api/ src/main.rs | head -10

# 6. Enumerate env vars with security implications
grep -rn "MASTER_KEY\|SECRET\|PASSWORD\|TOKEN\|SECURE_COOKIES\|TRUST_PROXY\|CORS\|DEV\b" src/config.rs | head -20

# 7. Docker security baseline
grep -n "USER\|EXPOSE\|--privileged\|capabilities\|securityContext" docker/Dockerfile* helm/platform/templates/deployment.yaml | head -20
```

Record all pre-flight results — agents will use this baseline context.

---

## Phase 1: Parallel Security Audit Agents

Launch **all 10 agents concurrently**. Each agent audits a specific attack surface.

**Critical instructions for EVERY agent prompt:**
- This is a **security audit** — think like an attacker. For each feature, ask: *"How would I abuse this?"*
- READ every file in scope completely — vulnerabilities hide in edge cases
- For each finding, describe the **attack scenario** (not just "this is bad")
- Output format: `[SEVERITY] file:line — vulnerability title\n  Attack: how to exploit\n  Impact: what the attacker gains\n  Fix: specific remediation`
- Agent is performing an AUDIT (read-only) — it must NOT edit any files
- Include the relevant CLAUDE.md security sections as context

---

### Agent 1: Authentication & Session Security

**Scope:** `src/auth/middleware.rs`, `src/auth/password.rs`, `src/auth/token.rs`, `src/auth/rate_limit.rs`, `src/auth/passkey.rs`, `src/auth/cli_creds.rs`, `src/auth/user_type.rs`, `src/auth/mod.rs`, `src/api/setup.rs`, `src/api/health.rs`, `src/api/cli_auth.rs`, `src/api/users.rs`, `src/api/passkeys.rs`

**Read ALL files, then analyze each attack vector:**

_Authentication bypass:_
- [ ] Can any API endpoint be reached without authentication? (Trace every route — which ones skip `AuthUser`?)
- [ ] Are there default credentials? Under what conditions? (Check `PLATFORM_DEV` gating)
- [ ] Can the setup endpoint be re-invoked after initial setup to create a new admin?
- [ ] Does the health endpoint leak internal state that aids an attacker?
- [ ] Can `AuthUser` extraction be fooled by malformed headers?

_Password security:_
- [ ] Is argon2 used with safe parameters? (Check memory/time/parallelism cost)
- [ ] Is comparison truly timing-safe? (Verify `dummy_hash()` path for non-existent users)
- [ ] Can password be set to empty or trivially short?
- [ ] Is password hash ever exposed in any API response or log?
- [ ] Salt generation: uses `argon2::password_hash::rand_core::OsRng`? (NOT `rand::rng()`)

_Token security:_
- [ ] Are API tokens hashed before storage? (SHA-256, not plaintext)
- [ ] Is token comparison done on hashes (timing-safe) or raw values?
- [ ] Can expired tokens still authenticate? (Check enforcement)
- [ ] Can revoked tokens still authenticate? (Check enforcement)
- [ ] Token expiry range: is max (365 days) too long? Is there forced rotation?
- [ ] Can token prefix `plat_` be used for enumeration?
- [ ] Are tokens logged anywhere (tracing, audit, error messages)?

_Session security:_
- [ ] Cookie flags: `HttpOnly`, `SameSite=Strict`, `Secure` when configured?
- [ ] Can session fixation attack succeed? (Is session ID regenerated on login?)
- [ ] Session cleanup: does deactivating a user kill ALL their sessions?
- [ ] Is there a maximum session lifetime (not just idle timeout)?
- [ ] Can an attacker create unlimited sessions (resource exhaustion)?

_Rate limiting:_
- [ ] Which endpoints have rate limiting? Which sensitive endpoints lack it?
- [ ] Can rate limit be bypassed via IP spoofing (`X-Forwarded-For` with `TRUST_PROXY`)?
- [ ] Is the rate limit window sufficient (too high a limit defeats the purpose)?
- [ ] Does rate limit use per-user or per-IP keys? Can it be distributed across IPs?
- [ ] Is there account lockout after N failures?

_WebAuthn/Passkeys:_
- [ ] Is relying party ID validated?
- [ ] Is attestation verified?
- [ ] Is challenge replay prevented?
- [ ] Can an attacker register a passkey on someone else's account?
- [ ] Is passkey removal properly authenticated?

_CLI auth:_
- [ ] Is the CLI auth flow resistant to CSRF?
- [ ] Is the device code / polling flow time-limited?
- [ ] Can the token exchange be replayed?

**Output:** Numbered findings with attack scenario.

---

### Agent 2: Authorization & Privilege Escalation

**Scope:** `src/rbac/resolver.rs`, `src/rbac/types.rs`, `src/rbac/delegation.rs`, `src/rbac/mod.rs`, `src/api/admin.rs`, `src/api/helpers.rs`, `src/workspace/mod.rs` (or all files under `src/workspace/`)

**Read ALL files, then analyze:**

_RBAC bypass:_
- [ ] Can a non-admin user call admin endpoints? (Check every admin endpoint for `require_admin()`)
- [ ] Can a user modify their own role/permissions?
- [ ] Can a user create a delegation that grants more permissions than they have?
- [ ] Does delegation chain allow transitive privilege escalation?
- [ ] Is there a confused deputy problem — can user A trick the system into acting with user B's permissions?

_Scope boundary bypass:_
- [ ] Can a scoped API token access resources outside its `boundary_project_id`?
- [ ] Can a scoped API token access resources outside its `boundary_workspace_id`?
- [ ] Does `has_permission_scoped()` correctly intersect token scopes with RBAC permissions?
- [ ] Can workspace membership be exploited to access private projects?

_Permission cache poisoning:_
- [ ] Can an attacker manipulate the Valkey permission cache to escalate?
- [ ] Is cache invalidation reliable? (What if a role change doesn't invalidate?)
- [ ] Is the cache key (`perms:{user_id}:{project_id}`) collision-resistant?
- [ ] TTL: can stale permissions persist longer than acceptable?

_IDOR (Insecure Direct Object Reference):_
- [ ] For every endpoint that takes a resource ID (project, issue, MR, webhook, secret, session): does it verify the caller has access to that specific resource?
- [ ] Can a user enumerate private project IDs by probing different UUIDs?
- [ ] Do error responses differ between "not found" and "not authorized"? (Should always be 404)

_Workspace permissions:_
- [ ] Can a user join a workspace they shouldn't? (Check membership creation auth)
- [ ] Does removing a user from a workspace revoke all derived permissions?
- [ ] Can a workspace member escalate from `member` to `admin`/`owner`?

_User deactivation:_
- [ ] When a user is deactivated: are ALL sessions killed? API tokens revoked? Permissions purged?
- [ ] Can a deactivated user re-authenticate before cleanup completes?
- [ ] Are deactivated user's resources (projects, issues) still accessible to others?

**Output:** Numbered findings with attack scenario.

---

### Agent 3: Injection & Input Validation

**Scope:** `src/validation.rs`, `src/pipeline/definition.rs`, `src/api/projects.rs`, `src/api/issues.rs`, `src/api/merge_requests.rs`, `src/api/webhooks.rs`, `src/api/secrets.rs`, `src/api/pipelines.rs`, `src/git/smart_http.rs` (or equivalent git protocol files), `src/git/ssh.rs` (or equivalent), `src/registry/blobs.rs`, `src/registry/manifests.rs`

**Read ALL files, then test every input path:**

_SQL injection:_
- [ ] Are ALL database queries parameterized? (Search for any string concatenation into SQL)
- [ ] Are there any `sqlx::query()` (dynamic) calls in `src/` that construct SQL from user input?
- [ ] Can ORDER BY, LIMIT, or column names be user-controlled?
- [ ] Can search/filter parameters inject into queries?

_Command injection:_
- [ ] Every place the platform spawns a subprocess or shell command: are arguments sanitized?
- [ ] `check_container_image()` — can a malicious image name inject commands?
- [ ] `check_setup_commands()` — can pipeline setup commands escape their sandbox?
- [ ] Git operations: can a branch name, tag, or commit message inject into git commands?
- [ ] Git hook execution: can hook content be controlled by a pusher?

_Path traversal:_
- [ ] Git file browsing: can `../../etc/passwd` be accessed?
- [ ] Ops repo paths: can a project name traverse to another project's repo?
- [ ] LFS object storage: can an OID traverse MinIO paths?
- [ ] Registry blob storage: can a digest traverse storage paths?
- [ ] Artifact paths in pipeline definitions: are they validated?
- [ ] Template rendering: can template variables inject path traversal?

_XSS (Cross-Site Scripting):_
- [ ] Are API responses that contain user-generated content (issue body, MR description, comments) properly escaped when rendered by the UI?
- [ ] Does the platform set `X-Content-Type-Options: nosniff` to prevent MIME sniffing?
- [ ] Are SVG uploads (if any) sanitized? (SVG can contain JavaScript)
- [ ] Markdown rendering: is it configured to strip HTML/script tags?

_SSRF (Server-Side Request Forgery):_
- [ ] Webhook URLs: is `validate_webhook_url()` applied to ALL user-supplied URLs the server fetches?
- [ ] Are there any NEW outbound HTTP calls (besides webhooks) that use user-supplied URLs?
- [ ] Can DNS rebinding bypass IP validation? (URL resolves to public IP initially, then to private IP)
- [ ] Can redirect-following bypass SSRF protection? (Verify no-redirect policy)
- [ ] Cloud metadata endpoint (169.254.169.254) blocked in all URL validation paths?

_Header injection:_
- [ ] Can user input end up in HTTP response headers? (e.g., redirect URLs, Content-Disposition)
- [ ] Can user input inject CRLF into headers?

_Email injection:_
- [ ] Can email recipients, subjects, or bodies be injected with additional headers?
- [ ] Can notification content inject malicious content into emails?

_ReDoS:_
- [ ] Do any validation regexes have catastrophic backtracking patterns?
- [ ] Are regexes applied to user input bounded by length limits first?

_Container image injection:_
- [ ] Can a pipeline definition specify an image that escapes the container?
- [ ] Can a malicious image name include registry credentials or auth tokens?
- [ ] Is there an allowlist of registries, or can any registry be used?

**Output:** Numbered findings with attack scenario.

---

### Agent 4: Secrets, Cryptography & Data Protection

**Scope:** `src/secrets/engine.rs`, `src/secrets/mod.rs`, `src/secrets/request.rs`, `src/secrets/types.rs`, `src/auth/password.rs`, `src/auth/token.rs`, `src/notify/webhook.rs`, `src/config.rs`, `src/api/secrets.rs`, `src/api/llm_providers.rs`

**Read ALL files, then analyze:**

_Encryption at rest:_
- [ ] AES-256-GCM: is nonce generated with a CSPRNG? Is nonce EVER reused? (Nonce reuse = catastrophic break)
- [ ] Is the nonce stored with the ciphertext? (Required for decryption)
- [ ] Is the ciphertext authenticated? (GCM provides this — verify it's not stripped)
- [ ] Key derivation: is `PLATFORM_MASTER_KEY` used directly or derived? (Should be derived via KDF)
- [ ] Can the master key be rotated? What happens to existing secrets? (Document the gap if none)
- [ ] Is zeroize used for sensitive memory? (Secret values, plaintext after encryption)
- [ ] Can a memory dump or core dump reveal plaintext secrets?

_Token hashing:_
- [ ] SHA-256 for API tokens: is it applied consistently (hash before store, hash before compare)?
- [ ] Is there a timing side channel in token lookup? (Constant-time comparison on hashes)
- [ ] Are old unhashed tokens still in the database? (Migration gap)

_Password hashing:_
- [ ] Argon2 parameters: are they OWASP-recommended? (memory ≥64MB, iterations ≥3, parallelism ≥1)
- [ ] Is the password hashing work factor configurable or hardcoded?

_HMAC (webhooks):_
- [ ] Is HMAC-SHA256 computed correctly? (key, message order)
- [ ] Is the HMAC comparison timing-safe?
- [ ] Can an attacker forge webhook deliveries if they know the signing secret?

_LLM provider API keys:_
- [ ] Are API keys encrypted at rest using the secrets engine?
- [ ] Can API keys be retrieved in plaintext via the API? (Should be masked)
- [ ] Are API keys logged in any tracing or error path?
- [ ] Can a user with `ProjectRead` access a project's LLM keys?

_Secret request flow:_
- [ ] Can a user bypass the approval chain to access a secret?
- [ ] Are approved secrets properly scoped (only the requester can use them)?
- [ ] Is the approval flow audited?

_Data protection:_
- [ ] What PII is stored? (emails, names, IP addresses) Is any of it encrypted?
- [ ] Is audit log data retention time-limited or unbounded?
- [ ] Can observability data (traces, logs) contain PII? Is it scrubbed?
- [ ] Are database backups encrypted? (May be out of scope for code audit — note if so)

_Configuration secrets:_
- [ ] Is `PLATFORM_MASTER_KEY` ever logged at startup?
- [ ] Are SMTP credentials ever logged?
- [ ] Is `PLATFORM_DEV=true` in any non-dev configuration?
- [ ] Are default passwords used anywhere outside dev mode?

**Output:** Numbered findings with attack scenario.

---

### Agent 5: Network Security, CORS & Transport

**Scope:** `src/main.rs`, `src/config.rs`, `helm/platform/templates/networkpolicy.yaml` (or equivalent), `helm/platform/templates/deployment.yaml`, `helm/platform/templates/service.yaml`, `helm/platform/templates/ingress.yaml`, `helm/platform/values.yaml`, `hack/cluster-up.sh`

**Read ALL files, then analyze:**

_TLS/Transport security:_
- [ ] Is TLS enforced for all external connections? (Or is it expected to terminate at ingress?)
- [ ] rustls used everywhere (no openssl)? Verify `deny.toml` ban.
- [ ] SMTP: is STARTTLS or implicit TLS enforced? Can it fall back to plaintext?
- [ ] Valkey connection: is TLS used? (Internal network — document the trust boundary)
- [ ] PostgreSQL connection: is TLS used? Is certificate verification enabled?
- [ ] MinIO connection: HTTP or HTTPS? Encrypted in transit?

_CORS:_
- [ ] Default CORS policy: is it deny-by-default? (Empty `PLATFORM_CORS_ORIGINS` = deny)
- [ ] Can a misconfigured CORS allow `*` origin? (Check parsing logic)
- [ ] Are credentials (cookies) allowed in CORS? With which origins?
- [ ] Is CORS enforced consistently on all routes (including WebSocket upgrade)?

_Security headers:_
- [ ] `X-Frame-Options: DENY` — prevents clickjacking
- [ ] `X-Content-Type-Options: nosniff` — prevents MIME sniffing
- [ ] `Referrer-Policy: strict-origin-when-cross-origin` — limits referrer leakage
- [ ] `Content-Security-Policy` — is one set? Does it allow `unsafe-inline`?
- [ ] `Strict-Transport-Security` (HSTS) — is it set when behind TLS termination?
- [ ] `Permissions-Policy` — is it set to disable unused browser features?
- [ ] Are these headers applied to ALL responses (API + UI + static)?

_Network segmentation (K8s):_
- [ ] NetworkPolicy: can pipeline pods reach the platform API? (They should for reporting)
- [ ] NetworkPolicy: can pipeline pods reach the internet? (Needed for image pull — but should they reach everything?)
- [ ] NetworkPolicy: can agent pods reach other agent pods? (They shouldn't)
- [ ] NetworkPolicy: can external traffic reach Postgres/Valkey/MinIO directly? (It shouldn't)
- [ ] NetworkPolicy: can pods in one project namespace access pods in another project namespace?
- [ ] Is there a default-deny policy in platform-managed namespaces?

_Ingress security:_
- [ ] Is rate limiting configured at the ingress level?
- [ ] Is WAF or request filtering configured?
- [ ] Are TLS certificates properly configured (not self-signed in production)?
- [ ] Is HTTP → HTTPS redirect configured?
- [ ] Are large request body limits enforced at ingress (before reaching the platform)?

_Port exposure:_
- [ ] Which ports does the platform listen on? Are any unnecessary?
- [ ] Are debug endpoints (pprof, metrics) exposed externally? (They shouldn't be)
- [ ] SSH port (2222): is it properly firewalled for external deployments?
- [ ] Kind cluster port mappings: do they expose anything unintended?

_Proxy trust:_
- [ ] `PLATFORM_TRUST_PROXY`: what happens if enabled with no reverse proxy? (Spoofable X-Forwarded-For)
- [ ] Is the trusted proxy set configurable (specific IP range) or all-or-nothing?
- [ ] Can rate limiting be bypassed by spoofing X-Forwarded-For when TRUST_PROXY is true?

**Output:** Numbered findings with attack scenario.

---

### Agent 6: Container & Kubernetes Security

**Scope:** `docker/Dockerfile`, `docker/Dockerfile.platform-runner`, `docker/Dockerfile.platform-runner-bare`, `docker/Dockerfile.dev-pod`, `helm/platform/templates/deployment.yaml`, `helm/platform/templates/clusterrole.yaml` (or RBAC templates), `src/agent/service.rs`, `src/agent/identity.rs`, `src/pipeline/executor.rs`, `src/deployer/applier.rs`

**Read ALL files, then analyze:**

_Container escape:_
- [ ] Do any containers run as root? (Check `USER` directive in Dockerfiles, `securityContext` in Helm)
- [ ] Are containers set to `readOnlyRootFilesystem: true`?
- [ ] Is `allowPrivilegeEscalation: false` set?
- [ ] Are unnecessary capabilities dropped? (`drop: [ALL]`)
- [ ] Can a pipeline step container mount the host filesystem?
- [ ] Can a pipeline step container access the K8s API?
- [ ] Can an agent pod escape to the host?

_K8s RBAC (ClusterRole):_
- [ ] Does the platform's ClusterRole have wildcard permissions (`*`)? On which resources?
- [ ] Can the platform create privileged pods? (Check if the ClusterRole allows it)
- [ ] Is the ClusterRole scoped to only the namespaces the platform manages?
- [ ] Can a user trick the platform into creating a pod with elevated privileges?
- [ ] Are there separate service accounts for different trust levels?

_Agent pod security:_
- [ ] What `securityContext` do agent pods get? (Check `src/agent/service.rs` pod spec)
- [ ] Can an agent pod access the Kubernetes API? (ServiceAccount token mounting)
- [ ] Can an agent pod access other pods in the same namespace?
- [ ] Can an agent pod access the host network?
- [ ] Resource limits: can a malicious agent exhaust node resources?
- [ ] Can an agent install arbitrary packages or tools? (If the container allows it)
- [ ] What is mounted into the agent pod? Can mounted volumes leak secrets?

_Pipeline pod security:_
- [ ] What `securityContext` do pipeline pods get? (Check `src/pipeline/executor.rs`)
- [ ] Can a pipeline step run arbitrary commands as root?
- [ ] Can a pipeline step access secrets from other projects?
- [ ] Is there network egress filtering for pipeline pods?
- [ ] Can a pipeline step modify its own pod spec (DagsHub-style escape)?
- [ ] Kaniko: is it properly sandboxed? Can it push to arbitrary registries?

_Image supply chain:_
- [ ] Are base images from trusted registries?
- [ ] Are image digests used instead of mutable tags for security-critical images?
- [ ] Can a user specify an arbitrary container image for pipeline steps? (Typosquatting, malicious images)
- [ ] Is there an image allowlist or scanning policy?

_Deployer security:_
- [ ] Can the deployer apply arbitrary manifests? (What prevents deploying a privileged pod?)
- [ ] Is server-side apply field manager correctly set? (Prevents field ownership conflicts)
- [ ] Can a malicious manifest in an ops repo compromise the cluster?
- [ ] Does the deployer validate manifests before applying?

_Secrets in K8s:_
- [ ] Are Kubernetes secrets used securely? (Not logged, not mounted unnecessarily)
- [ ] Is the platform service account token auto-mounted into pods that don't need it?
- [ ] Are secret env vars in agent/pipeline pods scoped correctly?

**Output:** Numbered findings with attack scenario.

---

### Agent 7: Git & Registry Protocol Security

**Scope:** All files under `src/git/` and `src/registry/`

**Read ALL files, then analyze:**

_Git smart HTTP:_
- [ ] Push authentication: can an unauthenticated user push? (Verify auth on receive-pack)
- [ ] Pull authorization: are private repos protected? (Verify auth on upload-pack)
- [ ] Can a force push bypass branch protection? (Server-side enforcement)
- [ ] Can a malicious pack file crash the server or cause OOM? (Size limits, streaming)
- [ ] Git protocol injection: can ref names contain shell metacharacters?

_Git SSH:_
- [ ] SSH key authentication: is key validation secure?
- [ ] Can a user authenticate with another user's key? (Key → user mapping correctness)
- [ ] User enumeration: does failed SSH auth leak whether a username exists?
- [ ] Can SSH command parsing be exploited? (Only `git-upload-pack` and `git-receive-pack` allowed)
- [ ] Shared RNG boundary: `russh` and platform don't share `rand` instances?

_LFS:_
- [ ] LFS authentication: are batch/upload/download endpoints authenticated?
- [ ] OID validation: can a malicious OID traverse MinIO storage? (Exactly 64 hex chars)
- [ ] Size limits: can a user upload arbitrarily large LFS objects?
- [ ] Can a user access another project's LFS objects by guessing OIDs?

_OCI registry:_
- [ ] Registry auth: are all push/pull operations authenticated?
- [ ] Scope validation: can a token scoped to project A push to project B's registry?
- [ ] Digest verification: is uploaded content verified against claimed digest? (Prevents content spoofing)
- [ ] Manifest injection: can a manifest reference blobs from other repositories?
- [ ] Cross-repo mounting: is authorization checked when mounting blobs across repos?
- [ ] Can a user delete another user's images?
- [ ] Tag mutability: can pushing to an existing tag cause supply chain confusion?

_Branch protection bypass:_
- [ ] Can branch protection be bypassed by pushing via SSH when it's enforced for HTTP?
- [ ] Can branch protection be bypassed by renaming the branch?
- [ ] Can an admin bypass branch protection? Is this intended and audited?
- [ ] Are protection patterns safe against ReDoS?

_Git hooks:_
- [ ] Server-side hooks: can a push inject malicious hook content?
- [ ] Pre-receive hooks: can they be disabled by a user?
- [ ] Hook timeout: is there one? Can a malicious hook hang forever?

**Output:** Numbered findings with attack scenario.

---

### Agent 8: Supply Chain & Dependency Security

**Scope:** `Cargo.toml`, `Cargo.lock`, `deny.toml`, `cli/agent-runner/Cargo.toml`, `cli/agent-runner/Cargo.lock`, `mcp/package.json`, `mcp/package-lock.json`, `ui/package.json`, `ui/package-lock.json`, `docker/Dockerfile*`, `.github/workflows/*.yaml`, `.pre-commit-config.yaml`, `.gitleaks.toml`

**Read ALL files, then analyze:**

_Rust dependency security:_
- [ ] `cargo audit` results: any known vulnerabilities? (Use pre-flight output)
- [ ] `deny.toml` configuration: are advisories, licenses, and bans all checked?
- [ ] Are there any `[patch]` or `[replace]` sections in Cargo.toml? (Potential supply chain injection)
- [ ] Are all dependencies from crates.io? (Check for git dependencies)
- [ ] Are there vendored/local crates? Are they reviewed?
- [ ] Is `unsafe_code = "forbid"` enforced?
- [ ] `openssl` / `openssl-sys` banned in deny.toml?

_JavaScript dependency security:_
- [ ] `npm audit` results: any known vulnerabilities?
- [ ] Are MCP server dependencies minimal? (Fewer deps = smaller attack surface)
- [ ] Are dependencies pinned in lock files?
- [ ] Are there any postinstall scripts that could be malicious?
- [ ] Is there a `.npmrc` with registry overrides?

_Docker supply chain:_
- [ ] Are base images from official registries?
- [ ] Are base image tags pinned to specific versions (not `latest`)?
- [ ] Are multi-stage builds used to avoid leaking build tools?
- [ ] Can a compromised base image compromise the platform?
- [ ] Are there any `curl | sh` patterns in Dockerfiles? (Verify with checksums)
- [ ] Is there a Docker image scanning step in CI?

_CI/CD supply chain:_
- [ ] GitHub Actions: are action versions pinned to SHA (not mutable tags)?
- [ ] Can a PR modify CI workflows to exfiltrate secrets?
- [ ] Are CI secrets (Docker registry creds, etc.) properly scoped?
- [ ] Is there branch protection on the main branch? (Prevents direct push)
- [ ] Can a malicious dependency's build script exfiltrate env vars during CI?

_Pre-commit / Git hooks:_
- [ ] Is gitleaks configured to detect all secret patterns?
- [ ] Can pre-commit hooks be bypassed with `--no-verify`?
- [ ] Are hook tool versions pinned?

_Dependency freshness:_
- [ ] Are there significantly outdated dependencies with security implications?
- [ ] Is there an automated dependency update mechanism? (Dependabot, Renovate)
- [ ] Are transitive dependencies audited? (Not just direct)

**Output:** Numbered findings with attack scenario.

---

### Agent 9: Agent & Pipeline Sandbox Security

**Scope:** All files under `src/agent/`, `src/pipeline/`, `cli/agent-runner/src/`, `tests/fixtures/mock-claude-cli.sh`

**Read ALL files, then analyze the trust boundary between user code and platform code:**

_Agent sandbox:_
- [ ] What can an agent session do? (Map the full capability set)
- [ ] Can an agent session access secrets from other projects?
- [ ] Can an agent session access the platform API beyond its project scope?
- [ ] Can an agent session read other agents' Valkey channels?
- [ ] Can an agent session modify its own permissions or extend its lifetime?
- [ ] Can an agent install packages that persist after the session ends?
- [ ] What happens if an agent tries to access K8s API from inside the pod?
- [ ] Is there a maximum session duration enforced?
- [ ] Can an agent session fork-bomb or exhaust disk?

_Agent Valkey ACL:_
- [ ] ACL rules: does the agent only get access to its own channels and keys?
- [ ] `resetkeys resetchannels -@all` baseline: is it applied?
- [ ] Can the agent SUBSCRIBE to a wildcard pattern and see other sessions' messages?
- [ ] Can the agent execute dangerous commands (FLUSHDB, CONFIG, DEBUG)?
- [ ] Is the ACL user cleaned up after session termination?

_Agent identity:_
- [ ] What API token does the agent get? What permissions does it have?
- [ ] Is the token boundary-scoped to the correct project?
- [ ] Is the token short-lived? What's the expiry?
- [ ] Can the agent create new API tokens or sessions?
- [ ] Can the agent escalate from its scoped token to a broader one?

_Pipeline sandbox:_
- [ ] What can a pipeline step do? (Map the full capability set)
- [ ] Can a pipeline step access the K8s API? (Service account token mounting)
- [ ] Can a pipeline step access other projects' data?
- [ ] Can a pipeline step modify the platform itself? (E.g., push a malicious image to the platform's own registry)
- [ ] Can a pipeline step access secrets that weren't explicitly injected?
- [ ] Is there CPU/memory limit enforcement on pipeline pods?
- [ ] What is the network egress policy for pipeline pods?
- [ ] Can a pipeline step run indefinitely? (Timeout enforcement)

_Pipeline definition security:_
- [ ] Can a `.platform.yaml` specify a privileged pod?
- [ ] Can a `.platform.yaml` mount host volumes?
- [ ] Can a `.platform.yaml` set securityContext overrides?
- [ ] Can a `.platform.yaml` reference services in other namespaces?
- [ ] Can step commands escape the intended execution context?
- [ ] Can environment variables in `.platform.yaml` reference secrets from other projects?

_Cross-session isolation:_
- [ ] Are agent pods in separate namespaces per project?
- [ ] Can two agents from the same project interfere with each other?
- [ ] Can a pipeline and an agent in the same project interfere?
- [ ] Is the git repo working copy properly isolated between sessions?

_Claude CLI security:_
- [ ] What tools/capabilities does the Claude CLI get inside the agent pod?
- [ ] Can Claude CLI access the network? (For MCP servers, webhooks)
- [ ] Can Claude CLI read/write files outside the project working directory?
- [ ] Are MCP server permissions properly scoped?
- [ ] Can the Claude CLI or MCP servers make arbitrary API calls to the platform?

**Output:** Numbered findings with attack scenario.

---

### Agent 10: Information Disclosure & Logging Security

**Scope:** Entire `src/` directory (search-based, not file-by-file), plus `src/config.rs`, `src/error.rs`, `src/audit.rs`, `src/observe/ingest.rs`, `src/observe/query.rs`, `ui/src/lib/api.ts`

**Scan ALL Rust source files for patterns that leak information:**

_Error message disclosure:_
- [ ] Grep for `Internal(` patterns: do any pass raw error messages to API responses?
- [ ] Grep for `format!` in `ApiError` implementations: any dynamic content that leaks internals?
- [ ] Are SQL errors ever surfaced to the client?
- [ ] Are file path errors ever surfaced to the client?
- [ ] Are stack traces ever returned in API responses?
- [ ] Do 500 errors have a generic message or leak the cause?

_Logging sensitive data:_
- [ ] Grep for `password` in `tracing::` calls: any password logging?
- [ ] Grep for `token` in `tracing::` calls: any token logging? (Distinguish from tracing correlation tokens)
- [ ] Grep for `secret` in `tracing::` calls: any secret value logging?
- [ ] Grep for `key` in `tracing::` calls: any API key or master key logging?
- [ ] Grep for `webhook.*url` in logging: are webhook URLs (which may contain tokens) logged?
- [ ] Are request/response bodies logged at debug level? (May contain secrets)

_Observability data leakage:_
- [ ] Can the OTLP ingest endpoint be used to inject false telemetry? (Authenticated?)
- [ ] Can observability query endpoints be used to read data from other projects?
- [ ] Do stored traces/logs contain PII (user IPs, emails, names)?
- [ ] Is there data retention / purging for observability data?
- [ ] Can alert rules be configured to exfiltrate data via alert destinations?

_API response over-sharing:_
- [ ] Do list endpoints return fields that should be restricted? (E.g., user email in public project member lists)
- [ ] Do error responses include timing information that aids enumeration?
- [ ] Are soft-deleted resources truly invisible in API responses? (404, not filtered from lists)
- [ ] Do audit log queries expose actions from other projects?

_UI information disclosure:_
- [ ] Are API errors displayed verbatim to users? (May contain internal details)
- [ ] Does the UI store sensitive data in localStorage? (Visible to XSS)
- [ ] Are debug/development features disabled in production builds?
- [ ] Does the UI source map expose internal code structure?
- [ ] Console.log statements in production: do any log sensitive data?

_Git information disclosure:_
- [ ] Can unauthenticated users enumerate private repositories?
- [ ] Can commit messages from private repos leak via search or API?
- [ ] Does the git browse API expose file contents from private repos to unauthorized users?
- [ ] Are `.env` or other sensitive files protected from being committed and browsed?

_Version / tech stack disclosure:_
- [ ] Does the `Server` header reveal technology? (Should not)
- [ ] Do error pages reveal framework version?
- [ ] Does the health endpoint reveal version info? (Acceptable for authenticated users only)

**Output:** Numbered findings with attack scenario.

---

## Phase 2: Synthesis

Once all 10 agents return, synthesize into a single report.

### Synthesis rules

1. **Deduplicate** — merge same issue from multiple agents, keep highest severity
2. **Prioritize** — CRITICAL and HIGH first. Always prioritize: auth bypass > privilege escalation > injection > information disclosure > hardening
3. **Categorize** — group findings by OWASP-style categories:
   - **A01: Broken Access Control** — RBAC bypass, IDOR, privilege escalation, scope bypass
   - **A02: Cryptographic Failures** — weak encryption, nonce reuse, missing encryption, key management
   - **A03: Injection** — SQL, command, path traversal, XSS, SSRF, header, email
   - **A04: Insecure Design** — missing security controls, logic flaws, insufficient sandbox
   - **A05: Security Misconfiguration** — insecure defaults, missing headers, verbose errors, unnecessary exposure
   - **A06: Vulnerable Components** — dependency CVEs, outdated packages, supply chain risks
   - **A07: Authentication Failures** — credential stuffing, session fixation, missing rate limit
   - **A08: Data Integrity Failures** — unsigned deployments, missing verification, untrusted deserialization
   - **A09: Logging & Monitoring Failures** — missing audit logs, sensitive data in logs, insufficient alerting
   - **A10: SSRF** — unvalidated outbound requests, cloud metadata access
   - **Container & K8s** — container escape, RBAC escalation, pod security, network segmentation
4. **Risk-score** — for CRITICAL/HIGH, include: Likelihood (Low/Medium/High) + Impact (Low/Medium/High/Critical)
5. **Be actionable** — every finding above LOW must have a specific, implementable fix
6. **Credit defenses** — note security controls that are well-implemented (motivates keeping them)
7. **Number every finding** — S1, S2, S3... (S for Security, distinguishing from A-prefix audit, E-prefix ecosystem, R-prefix review)

---

## Phase 3: Write Security Audit Report

Persist the report as `plans/security-audit-<YYYY-MM-DD>.md`.

### Report structure

```markdown
# Security Audit Report

**Date:** <today>
**Scope:** Full platform — Rust monolith, CLI, MCP servers, UI, Docker images, Helm chart, CI/CD, K8s deployment
**Auditor:** Claude Code (automated security audit)
**Pre-flight:** cargo audit ✓/✗ | npm audit ✓/✗ | gitleaks ✓/✗ | deny ✓/✗ | unsafe_code=forbid ✓/✗

## Executive Summary
- Security posture: STRONG / ACCEPTABLE / NEEDS HARDENING / CRITICAL GAPS
- {2-3 sentences on overall security health}
- Findings: X critical, Y high, Z medium, W low
- Top risks: {1-3 bullet points — what could go wrong in production}
- Key defenses: {1-3 bullet points — what's done well}
- Recommendation: {Ready for external exposure? What must be fixed first?}

## Attack Surface Map

| Surface | Components | Exposure | Findings |
|---|---|---|---|
| HTTP API | src/api/, ui/ | External | S1, S3 |
| Git HTTP/SSH | src/git/ | External | S5 |
| OCI Registry | src/registry/ | External | S7 |
| WebSocket | src/api/, ui/ | External | — |
| OTLP Ingest | src/observe/ | Internal | S9 |
| Agent Pods | src/agent/, cli/ | Sandboxed | S2, S4 |
| Pipeline Pods | src/pipeline/ | Sandboxed | S6 |
| Deployer | src/deployer/ | Internal | S8 |
| K8s Control Plane | helm/, src/ | Internal | S10 |
| Data Stores | Postgres, Valkey, MinIO | Internal | — |

## OWASP Category Statistics

| Category | Critical | High | Medium | Low | Total |
|---|---|---|---|---|---|
| A01: Broken Access Control | N | N | N | N | N |
| A02: Cryptographic Failures | N | N | N | N | N |
| A03: Injection | N | N | N | N | N |
| A04: Insecure Design | N | N | N | N | N |
| A05: Security Misconfiguration | N | N | N | N | N |
| A06: Vulnerable Components | N | N | N | N | N |
| A07: Authentication Failures | N | N | N | N | N |
| A08: Data Integrity Failures | N | N | N | N | N |
| A09: Logging & Monitoring | N | N | N | N | N |
| A10: SSRF | N | N | N | N | N |
| Container & K8s | N | N | N | N | N |
| **Total** | **N** | **N** | **N** | **N** | **N** |

## Security Strengths
- {Defense 1 — e.g., "Timing-safe password comparison with dummy_hash() for non-existent users prevents user enumeration" — where it's implemented}
- {Defense 2}
- ...

## Critical & High Findings (must address before external exposure)

### S1: [CRITICAL] {vulnerability title}
- **Category:** A01 / A02 / ... / Container & K8s
- **Component:** {component}
- **File:** `src/path/file.rs:42`
- **Attack scenario:** {Step-by-step: how an attacker would exploit this}
- **Impact:** {What the attacker gains — data access, privilege level, blast radius}
- **Likelihood:** Low / Medium / High
- **Remediation:** {Specific code change or configuration fix}
- **Found by:** Agent {N}

### S2: [HIGH] {title}
...

## Medium Findings (fix within current sprint)

### SN: [MEDIUM] {title}
- **Category:** ...
- **File:** `src/path/file.rs:42`
- **Attack scenario:** {brief}
- **Remediation:** {specific fix}

## Low Findings (defense-in-depth)

- [LOW] S{N}: `src/path/file.rs:10` — {title} → {fix}

## Component Security Summary

### Authentication & Sessions — {STRONG/ACCEPTABLE/WEAK}
{Assessment of auth security posture, key strengths and gaps}

### Authorization & RBAC — {STRONG/ACCEPTABLE/WEAK}
{Assessment of authz, delegation chains, scope enforcement}

### Input Validation & Injection — {STRONG/ACCEPTABLE/WEAK}
{Assessment of validation coverage, injection prevention}

### Secrets & Cryptography — {STRONG/ACCEPTABLE/WEAK}
{Assessment of encryption, key management, secret handling}

### Network & Transport — {STRONG/ACCEPTABLE/WEAK}
{Assessment of TLS, CORS, headers, network policies}

### Container & K8s — {STRONG/ACCEPTABLE/WEAK}
{Assessment of container security, RBAC, pod security}

### Agent & Pipeline Sandbox — {STRONG/ACCEPTABLE/WEAK}
{Assessment of sandbox isolation, capability restriction}

### Supply Chain — {STRONG/ACCEPTABLE/WEAK}
{Assessment of dependency security, CI/CD integrity}

### Information Disclosure — {STRONG/ACCEPTABLE/WEAK}
{Assessment of error handling, logging, data leakage}

## Trust Boundary Diagram

```
[External Users]
    │
    ├── HTTPS ──▶ [Ingress / Load Balancer]
    │                  │
    │                  ├──▶ [Platform API :8080] ◄── AuthUser + RBAC
    │                  │       ├── Postgres (TLS?)
    │                  │       ├── Valkey (ACL)
    │                  │       ├── MinIO
    │                  │       └── K8s API
    │                  │
    │                  ├──▶ [Git SSH :2222] ◄── SSH key auth
    │                  │
    │                  └──▶ [Web UI] ◄── Session cookie
    │
    ├── [Agent Pods] ◄── Scoped token + Valkey ACL
    │       ├── Platform API (limited)
    │       ├── Valkey (own channels only)
    │       └── MCP servers (local)
    │
    └── [Pipeline Pods] ◄── No platform API access?
            ├── Registry (push)
            └── External (image pull)
```

Document the actual trust boundaries you observed — highlight any unexpected flows.

## Recommended Remediation Plan

### Immediate — before external exposure
1. {S1: Fix ... — estimated effort}
2. ...

### Short-term — within 2 weeks
1. {SN: Fix ...}
2. ...

### Medium-term — within 1 month
1. {Hardening improvements}
2. ...

### Ongoing
1. {Dependency monitoring}
2. {Security testing in CI}
3. {Periodic re-audit schedule}
```

### Rules
- Every finding gets a unique ID (S1, S2, ...) — the S-prefix distinguishes from `/audit` (A-prefix), `/audit-ecosystem` (E-prefix), and `/review` (R-prefix)
- **Attack scenarios are mandatory** for CRITICAL and HIGH — describe the steps an attacker would take
- Include the trust boundary diagram — it reveals gaps in network segmentation
- The report must be self-contained — readable without conversation context
- Do NOT include INFO-level items in the findings (include them in Strengths only)
- Include the OWASP category table even if all categories show zero findings
- The report should answer: *"Is this platform safe to expose to the internet?"*

---

## Phase 4: Summary to User

After writing the report, provide a concise summary:

1. Overall security posture (one sentence)
2. Finding counts by severity
3. Top 3 most critical vulnerabilities (one line each with attack scenario)
4. Top 3 security strengths (one line each)
5. OWASP category with most findings
6. Answer: "Is the platform ready for external exposure?"
7. Path to the full report file
8. Suggested next steps (e.g., "Fix S1-S3, then re-audit auth surface")

---

## Usage Notes

- This skill focuses exclusively on **security** — for code quality use `/audit`, for integration contracts use `/audit-ecosystem`.
- Run all three (`/audit` + `/audit-ecosystem` + `/audit-security`) for a complete platform assessment.
- Expect 15-25 minutes for the full security audit (10 parallel agents).
- The audit is **read-only** — no files are modified. To fix findings, use `/dev`.
- For focused security audits, tell the orchestrator which attack surface to focus on (e.g., "just auth and RBAC") — it can skip irrelevant agents.
- This audit does NOT perform active exploitation — it's source code analysis only. Complement with penetration testing for runtime validation.
- Consider running after: auth/RBAC changes, new API endpoints, new container images, dependency updates, or before a release.
