# Security Audit Report

**Date:** 2026-03-24
**Scope:** Full platform — Rust monolith (146 files), Agent Runner CLI (11 files), MCP servers (15 files), Web UI (127 files), Docker images (4), Helm chart (24 files), CI/CD workflows, K8s deployment
**Auditor:** Claude Code (automated security audit, 10 parallel agents)
**Pre-flight:** cargo audit ✗ (not installed) | npm audit: 3 HIGH | gitleaks ✗ (not installed) | deny ✓ | unsafe_code=forbid ✓

## Executive Summary

- **Security posture: NEEDS HARDENING** — The platform has strong fundamentals (parameterized queries, timing-safe auth, proper token hashing, SSRF protection) but critical gaps in **container/K8s security**, **SSH protocol parity**, and **supply chain integrity** that must be addressed before external exposure.
- **Findings: 10 critical, 25 high, 33 medium, 28 low** (96 total; ~53 unique, ~43 overlap with `/audit` A-prefix or `/audit-ecosystem` E-prefix — see cross-reference table)
- **Top risks:**
  - SSH push bypasses ALL branch protection (CRITICAL — live exploit path)
  - Agent pods can create privileged containers and escape to the node (CRITICAL — no PodSecurityAdmission → fix with `baseline` PSA)
  - ClusterRole grants cluster-wide secrets read/write (CRITICAL — single compromise = full cluster)
  - GitHub Actions pinned to mutable tags, not SHA digests (CRITICAL — proven attack vector)
- **Key defenses:** Timing-safe login with dummy_hash, all SQL parameterized via sqlx, token SHA-256 hashing, SSRF protection on webhooks, Valkey ACL per-session isolation, proper env var sanitization in agent subprocess spawning
- **Recommendation:** NOT ready for external exposure. Fix all CRITICAL and HIGH findings first, especially the SSH branch protection bypass, K8s RBAC scope reduction, container security contexts, and supply chain pinning.
- **Unique findings (not in /audit or /audit-ecosystem):** 10 critical, 16 high, 17 medium, 10 low = **53 net-new**. 43 findings overlap with existing audits but include security-specific attack scenarios and OWASP categorization.

## Cross-Reference with Other Audits

Findings marked **[COVERED]** were already identified in the codebase audit (`A`-prefix) or ecosystem audit (`E`-prefix). The security audit adds attack scenarios and OWASP categorization but the underlying issue is the same. **[UNIQUE]** findings are net-new to this audit.

| Security Finding | Also In | Status |
|---|---|---|
| S1 SSH branch protection bypass | A3 | COVERED |
| S2 SSH post-push empty branches | A81 | COVERED |
| S3 Agent can create privileged pods | — | UNIQUE |
| S4 Agent privilege escalation + sudo | A38, E8 | COVERED (E8 marked ACCEPTED — security audit disagrees, see S4) |
| S5 Agent RBAC secrets access | — | ACCEPTED (DD-2) |
| S6 ClusterRole cluster-wide secrets | — | UNIQUE |
| S7 Actions not SHA-pinned | — | UNIQUE |
| S8 kubectl no checksum | E31 | COVERED |
| S9 curl\|bash NodeSource | — | UNIQUE |
| S10 No CI permissions block | — | UNIQUE |
| S11 Deleted workspace permissions | — | UNIQUE |
| S12 Postgres no TLS | — | UNIQUE |
| S13 Valkey no TLS/auth | — | UNIQUE |
| S14 Audit log no auth | A1 | COVERED |
| S15 Pipeline steps as root | — | UNIQUE |
| S16 Pipeline registry no tag pattern | — | UNIQUE |
| S17 receive-pack OOM | A19 | COVERED |
| S18 Registry blob OOM | A21, A83 | COVERED |
| S19 Deployer unvalidated pod specs | — | UNIQUE (A18 covers Gateway only, not general pod spec) |
| S20 Workspace owner demotion | — | UNIQUE |
| S21 Observe cross-project data | — | UNIQUE |
| S22 Role permission cache invalidation | A26 | COVERED |
| S23 Pipeline pods bypass NetworkPolicy | — | UNIQUE |
| S24 No default-deny NetworkPolicy | — | UNIQUE |
| S25 trustProxy defaults true | — | UNIQUE |
| S26 Platform runs as root | E7 | COVERED |
| S27 Unpinned npm claude-code | E9 | COVERED |
| S28 kaniko :latest | E9 | COVERED |
| S29 API token on disk | E17 | COVERED |
| S30 ClusterRole namespace/RBAC write | — | UNIQUE |
| S31 Git auth token in env | — | UNIQUE |
| S32 Passkey DoS | A23 | COVERED |
| S33 Host path mount no dev gate | — | UNIQUE |
| S34 MCP npm no --ignore-scripts | — | UNIQUE |
| S35 No cargo-audit in CI | — | UNIQUE |
| S36 Password change no current pwd | E28 (UI side) | PARTIALLY COVERED |
| S37 Login rate limit on username | A65 | COVERED |
| S38 Rate limit INCR/EXPIRE race | A64 | COVERED |
| S39 begin_login no rate limit | — | UNIQUE |
| S40 Config Debug leaks | A16 | COVERED |
| S41 No zeroize | A85 | COVERED |
| S42 Webhook URLs logged | A25, A58 | COVERED |
| S43 Secret read no audit | — | UNIQUE |
| S44 No master key rotation | — | UNIQUE |
| S45 No workspace scope enforcement | — | UNIQUE |
| S46 Transitive delegation | A6 | COVERED |
| S47 Delegation revocation IDOR | — | UNIQUE |
| S48 Dev mode predictable creds | — | UNIQUE (A/E note it but as lower severity) |
| S49 Header injection releases | A46 | COVERED |
| S50 Missing CSP | A8 | COVERED |
| S51 Missing HSTS | — | UNIQUE |
| S52 No Git HTTP auth rate limit | — | UNIQUE |
| S53 No registry auth rate limit | — | UNIQUE |
| S54 OCI tags mutable | — | UNIQUE |
| S55 MinIO HTTP | — | UNIQUE |
| S56 Helm master key randAlphaNum | E3a | COVERED |
| S57 No prod namespace NetworkPolicy | — | UNIQUE |
| S58 Data store NP too broad | — | UNIQUE |
| S59 Proxy trust no CIDR | — | UNIQUE |
| S60 Pipeline images not validated | A7 | COVERED |
| S61 Git merge stderr leaks | — | UNIQUE |
| S62 Observe cross-project (dup S21) | — | (merged with S21) |
| S63 dispatch_single SSRF | — | UNIQUE |
| S64 NodePort fallback | — | UNIQUE |
| S65 Agent token 24h expiry | — | UNIQUE |
| S66 MCP npm vulnerabilities | — | UNIQUE |
| S67 MCP not in Dependabot | E57 | COVERED |
| S68 Suppressed RSA advisory | — | UNIQUE |
| S69 Logout deletes all sessions | A24 | COVERED |
| S70 Argon2 default params | — | UNIQUE |
| S71 Token expiry 365d max | — | UNIQUE |
| S74 ILIKE wildcards | A53 | COVERED |
| S75 trigger git_ref not validated | A73 | COVERED |
| S78 No image allowlist | — | UNIQUE |
| S79 Unicode check_name | A12 | COVERED |
| S88 Force push fail-open | A82 | COVERED |
| S89 expect panic NULL workspace | — | UNIQUE |
| S90/91 LFS size/count | A20 | COVERED |
| S92 Blob into memory | A21 | COVERED |
| S93 Tag name not validated | — | UNIQUE |
| S95 localStorage token | E44 | COVERED |

**Summary:** ~30 findings overlap with existing audits. ~53 findings are unique to the security audit, primarily in: K8s RBAC/NetworkPolicy, sandbox escape paths, supply chain integrity, transport encryption, rate limiting gaps, and RBAC edge cases.

## Attack Surface Map

| Surface | Components | Exposure | Critical Findings |
|---|---|---|---|
| HTTP API | src/api/, ui/ | External | S11, S14, S20 |
| Git HTTP | src/git/smart_http.rs | External | S17 |
| Git SSH | src/git/ssh_server.rs | External | **S1, S2** |
| OCI Registry | src/registry/ | External | S18 |
| WebSocket/SSE | src/api/, ui/ | External | — |
| OTLP Ingest | src/observe/ | Internal | S21 |
| Agent Pods | src/agent/, cli/ | Sandboxed | **S3, S4, S5** |
| Pipeline Pods | src/pipeline/ | Sandboxed | S15, S16 |
| Deployer | src/deployer/ | Internal | S19 |
| K8s Control Plane | helm/, src/ | Internal | **S6** |
| Data Stores | Postgres, Valkey, MinIO | Internal | S12, S13 |
| CI/CD | .github/workflows/ | Build-time | **S7, S8, S9, S10** |

## OWASP Category Statistics

| Category | Critical | High | Medium | Low | Total |
|---|---|---|---|---|---|
| A01: Broken Access Control | 1 | 7 | 5 | 2 | 15 |
| A02: Cryptographic Failures | 0 | 0 | 4 | 2 | 6 |
| A03: Injection | 0 | 1 | 3 | 5 | 9 |
| A04: Insecure Design | 3 | 4 | 4 | 2 | 13 |
| A05: Security Misconfiguration | 1 | 3 | 5 | 3 | 12 |
| A06: Vulnerable Components | 4 | 5 | 3 | 2 | 14 |
| A07: Authentication Failures | 0 | 2 | 4 | 2 | 8 |
| A08: Data Integrity Failures | 1 | 2 | 1 | 0 | 4 |
| A09: Logging & Monitoring | 0 | 1 | 3 | 4 | 8 |
| A10: SSRF | 0 | 0 | 1 | 1 | 2 |
| Container & K8s | 0 | 0 | 0 | 5 | 5 |
| **Total** | **10** | **25** | **33** | **28** | **96** |

## Security Strengths

1. **Timing-safe password verification** — `dummy_hash()` always runs argon2 for non-existent users, preventing user enumeration via timing (`src/auth/password.rs:19`)
2. **All SQL parameterized** — Every query in `src/` uses `sqlx::query!` or parameterized `sqlx::query()`. Zero string concatenation into SQL. Zero SQL injection vectors found.
3. **Token SHA-256 hashing before storage** — API tokens and session tokens are properly hashed before DB storage. Raw tokens never persisted (`src/auth/token.rs:20-24`)
4. **SSRF protection on webhooks** — Private IPs, link-local, loopback, cloud metadata, and non-HTTP schemes all blocked. No redirect following (`src/api/webhooks.rs`, `src/validation.rs`)
5. **Valkey ACL per-session isolation** — Each agent session gets a unique Valkey ACL user with `resetkeys resetchannels -@all` baseline, UUID-scoped channel patterns, and proper cleanup on termination (`src/agent/valkey_acl.rs`)
6. **Agent env var isolation** — `SubprocessTransport::spawn()` uses `env_clear()` then applies only whitelisted vars. `DATABASE_URL` and `PLATFORM_MASTER_KEY` never reach CLI subprocesses.
7. **Agent permission intersection** — Agent permissions computed as `role_perms INTERSECT spawner_perms` — an agent can never exceed its human spawner's access level (`src/agent/identity.rs`)
8. **Reserved env var protection** — Both agent and pipeline pods block project secrets from overriding critical vars like `PLATFORM_API_TOKEN`, `GIT_AUTH_TOKEN`, `PATH`
9. **AES-256-GCM with CSPRNG nonces** — Encryption uses 12-byte random nonces via `rand::fill()`, nonce stored with ciphertext, proper authenticated encryption (`src/secrets/engine.rs`)
10. **Setup endpoint properly gated** — Checks user count > 0, returns 404, consumes token atomically via `UPDATE WHERE used_at IS NULL`, rate limited to 3/5min (`src/api/setup.rs`)

---

## Critical & High Findings (must address before external exposure)

### S1: [CRITICAL] SSH push bypasses ALL branch protection rules — *also A3*
- **Category:** A08: Data Integrity Failures
- **Component:** Git SSH server
- **File:** `src/git/ssh_server.rs:222-266`
- **Attack scenario:** Attacker with write access pushes to a protected branch (`main`) via SSH. The HTTP path calls `enforce_push_protection()` at `smart_http.rs:472`, but the SSH `exec_request` handler spawns `git receive-pack` directly without any protection check. Force push to `main`, bypass `require_pr` rules, overwrite any protected branch.
- **Impact:** Complete nullification of branch protection for any user with SSH key access. Force pushes, direct-to-main commits, and PR bypass all possible.
- **Likelihood:** High — SSH is a standard git transport
- **Remediation:** In `exec_request()`, for write operations, parse pack data from stdin and call `enforce_push_protection()` before piping to git. Or install a server-side `pre-receive` hook.
- **Found by:** Agent 7

### S2: [CRITICAL] SSH post-push hooks receive empty branch/tag data — *also A81*
- **Category:** A08: Data Integrity Failures
- **Component:** Git SSH server
- **File:** `src/git/ssh_server.rs:349-362`
- **Attack scenario:** Push via SSH to a branch with a pipeline defined. `handle_post_push` constructs `PostReceiveParams` with `pushed_branches: Vec::new()`. Pipelines not triggered for non-default branches, MR head_sha not updated, stale reviews not dismissed.
- **Impact:** Stale MR approvals persist after SSH push. Pipelines silently skip. Webhooks fire for wrong branch.
- **Likelihood:** High
- **Remediation:** Parse pack data from SSH stdin stream to extract `ref_updates`, then pass correct `pushed_branches` and `pushed_tags`.
- **Found by:** Agent 7

### S3: [CRITICAL] Agent pods can create privileged containers — no PodSecurityAdmission
- **Category:** A04: Insecure Design
- **Component:** Agent sandbox
- **File:** `src/deployer/namespace.rs:43-63`
- **Attack scenario:** The `agent-edit` Role grants `verbs: ["*"]` on `pods`. Via kubectl (or K8s API via mounted SA token), the agent creates a pod with `privileged: true`, `hostNetwork: true`, `hostPID: true`, or hostPath volume mounts. No PodSecurityAdmission policy prevents this.
- **Impact:** Container escape to the node. Access to host filesystem, kubelet credentials, pivot to entire cluster.
- **Likelihood:** Medium (requires prompt injection or malicious user)
- **Remediation:** Apply PodSecurity Admission **`baseline`** profile (not `restricted`) on session namespaces. See implementation notes below.
- **Found by:** Agent 9

#### Implementation: Use `baseline`, not `restricted`

The `restricted` PSA profile is too strict for agent workloads. It enforces `drop: ["ALL"]` capabilities, `runAsNonRoot: true`, seccomp profiles, and restricted volume types. Agents spin up generic Docker images (which often default to root) and need to compile code, build images, and install packages — `restricted` would block most of these pods from launching.

**Use `baseline` instead** — it blocks the known privilege escalation vectors (which is the actual threat in this finding) while allowing standard pod execution:

| | `baseline` blocks (mitigates S3) | `baseline` allows (keeps agents functional) |
|---|---|---|
| ✓ | `privileged: true` | Running as root inside container |
| ✓ | `hostNetwork: true` | Default Linux capabilities |
| ✓ | `hostPID: true`, `hostIPC: true` | Standard volume types (emptyDir, PVC, configMap, secret) |
| ✓ | `hostPath` volume mounts | Normal container execution |
| ✓ | Dangerous capabilities (SYS_ADMIN, NET_RAW, etc. via explicit adds) | |

Apply in `src/deployer/namespace.rs` when creating session namespaces:
```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: agent-session-xyz
  labels:
    pod-security.kubernetes.io/enforce: baseline
    pod-security.kubernetes.io/enforce-version: latest
    # Warn on restricted violations without blocking — visibility into what would break
    pod-security.kubernetes.io/warn: restricted
    pod-security.kubernetes.io/warn-version: latest
```

#### Additional sandbox hardening (complement PSA)

PSA stops the node escape, but a complete sandbox also needs:

1. **NetworkPolicy (default-deny egress to cluster CIDRs)** — prevents the agent from scanning/attacking internal APIs and databases. The existing agent-session NetworkPolicy already does this (blocks 10/8, 172.16/12, 192.168/16) but needs to also cover pipeline pods (see S23).

2. **ResourceQuota on session namespaces** — without this, an agent can create thousands of pods and exhaust cluster resources. Add a ResourceQuota limiting total CPU, memory, and pod count per session namespace (e.g., 4 CPU, 8Gi memory, 10 pods max).

3. **LimitRange on session namespaces** — ensures every pod created by the agent has default resource requests/limits, preventing a single pod from consuming the entire quota.

### S4: [CRITICAL] Agent main container allows privilege escalation + passwordless sudo — *also A38, E8*
- **Category:** A04: Insecure Design
- **Component:** Agent sandbox
- **Files:** `src/agent/claude_code/pod.rs:30-34`, `docker/Dockerfile.platform-runner:57`
- **Attack scenario:** Agent runs with `allowPrivilegeEscalation: true`, no capability drops, and the image has `agent ALL=(ALL) NOPASSWD: ALL` in sudoers. Prompt injection or malicious code escalates to root, uses raw sockets (NET_RAW), exploits kernel vulnerabilities.
- **Impact:** Full root within agent container. Combined with mounted SA token and agent-edit Role, can create privileged pods (see S3).
- **Likelihood:** Medium
- **Remediation:** Drop ALL capabilities, set `allowPrivilegeEscalation: false`, remove sudo. Use init containers for package installs.
- **Note:** E8 marked this as ACCEPTED design trade-off. Security audit **disagrees** — the combination of sudo + no capability drops + secrets access (S5) + create privileged pods (S3) creates a full escape chain that sudo alone wouldn't enable. The isolation boundary (container + namespace + NP) is insufficient without PodSecurityAdmission (S3).
- **Found by:** Agent 6, Agent 9

### ~~S5: [CRITICAL] Agent RBAC Role grants secrets read/write in session namespace~~ → ACCEPTED
- **Category:** A01: Broken Access Control
- **Component:** Agent sandbox
- **File:** `src/deployer/namespace.rs:43-63`
- **Attack scenario:** The `agent-edit` Role grants `verbs: ["*"]` on `secrets`. Agent pod reads the registry push secret (containing Docker auth with API token) and any other K8s Secrets in the session namespace.
- **Status:** **Accepted design trade-off.** See `docs/design-decisions.md` DD-2 for full rationale.
- **Summary:** Agents need full Secrets CRUD to do standard K8s development work (create app secrets, debug deployments, clean up). Session namespaces are single-tenant and ephemeral. Registry push secret is scoped by tag pattern (`{project}/session-{id}-*`) and expires with the session. RBAC is namespace-scoped — no cross-namespace access.
- **Found by:** Agent 9

### S6: [CRITICAL] ClusterRole grants cluster-wide secrets access
- **Category:** A05: Security Misconfiguration
- **Component:** K8s RBAC
- **File:** `helm/platform/templates/clusterrole.yaml:38-40`
- **Attack scenario:** The platform's ClusterRole grants get/list/watch/create/update/patch/delete on `secrets` in ALL namespaces. If the platform pod is compromised, the attacker reads every Secret in the cluster — database creds, TLS keys, SA tokens, Helm release secrets from all applications.
- **Impact:** Full cluster credential compromise from a single pod compromise.
- **Likelihood:** Low (requires platform pod compromise first)
- **Remediation:** Shared ClusterRole + per-namespace RoleBinding pattern (Option C). See implementation notes below.
- **Found by:** Agent 6

#### Implementation: Shared ClusterRole + per-namespace RoleBinding

The platform needs secrets CRUD in every namespace it manages (project-dev, project-staging, project-prod, session namespaces) for syncing project secrets, registry pull secrets, agent tokens, and git auth tokens. But it does NOT need secrets access in `kube-system`, other tenants' namespaces, or Helm release secrets.

**Approach:** Use the ClusterRole as a reusable definition template, but only bind it via namespace-scoped RoleBindings in managed namespaces. No ClusterRoleBinding for secrets.

**Step 1 — Split the ClusterRole.** Remove `secrets` from the main ClusterRole. Create a separate ClusterRole that is only a definition (never bound cluster-wide):

```yaml
# helm/platform/templates/clusterrole-secrets.yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "platform.fullname" . }}-secrets-manager
  labels: {{ include "platform.labels" . | nindent 4 }}
rules:
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
```

No ClusterRoleBinding for this — it's just a reusable template.

**Step 2 — Bind per namespace.** In `src/deployer/namespace.rs`, when creating any managed namespace (project-dev, project-staging, project-prod, session namespaces), create a RoleBinding that grants the platform SA secrets access in ONLY that namespace:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: platform-secrets-access
  namespace: myproject-dev          # ← scoped to THIS namespace only
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole                 # ← references the shared template
  name: platform-secrets-manager
subjects:
  - kind: ServiceAccount
    name: platform                  # ← platform's own SA
    namespace: platform             # ← SA lives in the platform namespace
```

**Step 3 — Keep RBAC bootstrap in ClusterRole.** The main ClusterRole retains `roles` and `rolebindings` create/delete cluster-wide — the platform needs this to bootstrap its own per-namespace access when creating new namespaces. This is a much smaller surface than cluster-wide secrets.

**Result:**

| Namespace | Secrets access? | Why |
|---|---|---|
| `myproject-dev` | ✓ | Platform created it → RoleBinding exists |
| `myproject-staging` | ✓ | Platform created it → RoleBinding exists |
| `myproject-prod` | ✓ | Platform created it → RoleBinding exists |
| `session-abc123` | ✓ | Platform created it → RoleBinding exists |
| `kube-system` | ✗ | No RoleBinding → no access |
| `other-tenant` | ✗ | No RoleBinding → no access |
| `platform` | ✗ | No RoleBinding → platform's own secrets come from Helm, not from the SA |

**What stays in the main ClusterRole (non-secrets):**
- `namespaces` — create/delete (cluster-scoped by nature)
- `roles`, `rolebindings` — create/delete (needed to bootstrap per-namespace access)
- `pods`, `services`, `deployments`, `configmaps`, etc. — could also be scoped per-namespace using the same pattern in a follow-up, but secrets is the highest-priority item since it's the most sensitive resource type

### S7: [CRITICAL] GitHub Actions pinned to mutable tags, not SHA digests
- **Category:** A06: Vulnerable Components
- **Component:** CI/CD
- **File:** `.github/workflows/ci.yaml`, `.github/workflows/release.yaml`
- **Attack scenario:** All Actions use mutable tags (`actions/checkout@v6`, `codecov/codecov-action@v5`, etc.). A compromised upstream action (proven attack vector: codecov 2021, tj-actions 2025) exfiltrates `GITHUB_TOKEN`, `CODECOV_TOKEN`, and any CI secrets.
- **Impact:** Code injection into builds, secret exfiltration, malicious image push to GHCR (release workflow has `packages: write`).
- **Likelihood:** Medium (active attack vector in the ecosystem)
- **Remediation:** Pin every action to full SHA: `uses: actions/checkout@<sha> # v6`
- **Found by:** Agent 8

### S8: [CRITICAL] kubectl downloaded without checksum verification — *also E31*
- **Category:** A06: Vulnerable Components
- **Component:** Docker images
- **File:** `docker/Dockerfile.platform-runner:33-34`, `docker/Dockerfile.dev-pod:34-35`
- **Attack scenario:** MITM or DNS poisoning on dl.k8s.io serves a trojanized kubectl. The download uses `curl -sLO` with no SHA256 check. Version determined dynamically via `stable.txt`.
- **Impact:** Full cluster compromise — kubectl runs with pod ServiceAccount. Code execution inside every agent pod.
- **Likelihood:** Low (requires MITM during build)
- **Remediation:** Pin kubectl version and verify SHA256 checksum.
- **Found by:** Agent 8

### S9: [CRITICAL] curl | bash anti-pattern for NodeSource in dev-pod
- **Category:** A06: Vulnerable Components
- **Component:** Docker images
- **File:** `docker/Dockerfile.dev-pod:38`
- **Attack scenario:** `curl -fsSL https://deb.nodesource.com/setup_22.x | bash -` pipes untrusted remote script into root shell. Compromised nodesource.com or MITM delivers arbitrary code.
- **Impact:** Full compromise of dev-pod image.
- **Likelihood:** Low
- **Remediation:** Use official Node.js Docker image as build stage, or install via apt with pre-verified GPG key.
- **Found by:** Agent 8

### S10: [CRITICAL] No top-level permissions restriction in CI workflow
- **Category:** A06: Vulnerable Components
- **Component:** CI/CD
- **File:** `.github/workflows/ci.yaml`
- **Attack scenario:** CI workflow lacks a top-level `permissions: {}` block. On `pull_request` triggers, default token permissions may include `contents: write` depending on repo settings. A malicious PR can exploit actions that use the token.
- **Impact:** Potential write access to repo contents from a PR context.
- **Likelihood:** Medium
- **Remediation:** Add `permissions: {}` at top level, then grant minimal permissions per-job.
- **Found by:** Agent 8

### S11: [HIGH] Deleted workspace still grants project permissions permanently
- **Category:** A01: Broken Access Control
- **Component:** RBAC
- **File:** `src/rbac/resolver.rs:194-199`
- **Attack scenario:** Admin soft-deletes a workspace. The `add_workspace_permissions()` query joins `workspace_members → projects` but never checks `workspaces.is_active = true`. Since `workspace_members` rows are never deleted, every member retains ProjectRead/Write on all workspace projects — permanently.
- **Impact:** Revoked workspace members retain access indefinitely.
- **Remediation:** Add `JOIN workspaces w ON w.id = wm.workspace_id AND w.is_active = true`. Delete `workspace_members` or invalidate caches on workspace deletion.
- **Found by:** Agent 2

### S12: [HIGH] PostgreSQL connection uses no TLS
- **Category:** A02: Cryptographic Failures (mapped to A05 for network)
- **Component:** Data stores
- **File:** `src/store/pool.rs`
- **Attack scenario:** Attacker on the same network segment MITMs the plaintext Postgres wire protocol. Reads/modifies SQL traffic including credentials, session tokens, and user data.
- **Impact:** Full database credential and data interception.
- **Remediation:** TLS everywhere — production and dev/test. See implementation plan below.
- **Found by:** Agent 5

#### Implementation: TLS everywhere with self-signed certs in dev

sqlx already has `runtime-tokio-rustls` enabled and the rustls crypto provider is installed in `main.rs:60`. The infrastructure is ready — just not wired up. sqlx parses `sslmode` directly from the DATABASE_URL, so `src/store/pool.rs` and `src/config.rs` need **zero code changes**.

**Dev/Test cluster (`just cluster-up`, integration, E2E):**

Add an init container to `hack/test-manifests/postgres.yaml` that generates a self-signed cert at pod startup, then enable SSL via postgres args:

```yaml
initContainers:
  - name: generate-certs
    image: postgres:16-alpine
    command: ["sh", "-c"]
    args:
      - |
        openssl req -new -x509 -days 3650 -nodes \
          -subj "/CN=postgres" \
          -keyout /certs/server.key -out /certs/server.crt
        chmod 600 /certs/server.key
        chown 999:999 /certs/server.key /certs/server.crt
    volumeMounts:
      - name: certs
        mountPath: /certs
containers:
  - name: postgres
    image: postgres:16-alpine
    args:
      - "-c"
      - "max_connections=300"
      - "-c"
      - "ssl=on"
      - "-c"
      - "ssl_cert_file=/certs/server.crt"
      - "-c"
      - "ssl_key_file=/certs/server.key"
    volumeMounts:
      - name: certs
        mountPath: /certs
        readOnly: true
volumes:
  - name: certs
    emptyDir: {}
```

Then in `hack/test-in-cluster.sh`, append `?sslmode=require` to the DATABASE_URL:

```bash
# Before:
export DATABASE_URL="postgres://platform:dev@${NODE_IP}:${PG_PORT}/platform_dev"
# After:
export DATABASE_URL="postgres://platform:dev@${NODE_IP}:${PG_PORT}/platform_dev?sslmode=require"
```

Self-signed certs work fine with `sslmode=require` — it encrypts traffic without verifying the CA, which is appropriate for dev/test (no real MITM threat on localhost, but catches TLS-related bugs early).

**Production (Helm chart):**

Bitnami PostgreSQL chart has built-in TLS support. Enable in `helm/platform/values.yaml`:

```yaml
postgresql:
  tls:
    enabled: true
    autoGenerated: true   # Bitnami auto-generates self-signed certs
```

Then append `?sslmode=require` to the DATABASE_URL in `helm/platform/templates/secret.yaml`:

```
DATABASE_URL: postgres://platform:PASSWORD@release-postgresql:5432/platform?sslmode=require
```

For production with a real CA (follow-up): add `PLATFORM_DB_CA_CERT` env var, mount the CA cert from a K8s Secret, and use `?sslmode=verify-ca&sslrootcert=/path/to/ca.crt`.

**Changes summary:**

| File | Change | Effort |
|---|---|---|
| `hack/test-manifests/postgres.yaml` | Init container for self-signed certs, SSL args | Small |
| `hack/test-in-cluster.sh` | `?sslmode=require` on DATABASE_URL | 1 line |
| `hack/cluster-up.sh` | Same — uses the same postgres manifest | Already covered |
| `helm/platform/values.yaml` | `postgresql.tls.enabled: true, autoGenerated: true` | 2 lines |
| `helm/platform/templates/secret.yaml` | Append `?sslmode=require` to DATABASE_URL | 1 line |
| `src/store/pool.rs` | **No change** — sqlx reads sslmode from URL | None |
| `src/config.rs` | **No change** — URL passthrough | None |

### S13: [HIGH] Valkey connection has no TLS and no authentication
- **Category:** A05: Security Misconfiguration
- **Component:** Data stores
- **File:** `src/store/valkey.rs`, `helm/platform/values.yaml`
- **Attack scenario:** Valkey on plaintext port 6379 with auth disabled. Attacker with network access reads/writes permission caches, rate limit counters, and pub/sub events. Can forge permission cache entries to escalate privileges.
- **Impact:** Permission escalation, rate limit bypass, session data tampering.
- **Remediation:** Enable Valkey auth (`valkey.auth.enabled: true`), use `rediss://` for TLS.
- **Found by:** Agent 5

### S14: [HIGH] Audit log endpoint has no authorization — any user sees all platform audit entries — *also A1*
- **Category:** A01: Broken Access Control
- **Component:** API
- **File:** `src/api/dashboard.rs:139-191`
- **Attack scenario:** Any authenticated user calls `GET /api/audit-log` and sees ALL audit log entries across all projects and all users. The handler uses `_auth: AuthUser` — auth struct extracted but never checked.
- **Impact:** Complete visibility into every mutation: project creation, MR merges, role assignments, secret operations, deployments.
- **Remediation:** Add `require_admin(&state, &auth).await?` or project-scoped filtering.
- **Found by:** Agent 10

### S15: [HIGH] Pipeline step containers run as root with no security context
- **Category:** A04: Insecure Design
- **Component:** Pipeline sandbox
- **Files:** `src/pipeline/executor.rs:1337-1401`
- **Attack scenario:** Pipeline step containers have NO securityContext. Any user-specified image in `.platform.yaml` runs as root with full default capabilities (NET_RAW, SETUID, etc.). Comment says "kaniko needs root" but applies to ALL steps.
- **Impact:** Root access in pipeline containers with default capabilities. Network attacks via NET_RAW, exploitation of container runtime vulnerabilities.
- **Remediation:** Only allow root for `imagebuild` steps. All other step types: `runAsNonRoot: true`, drop ALL capabilities.
- **Found by:** Agent 6, Agent 9

### S16: [HIGH] Pipeline registry token has no tag pattern restriction
- **Category:** A01: Broken Access Control
- **Component:** Pipeline sandbox
- **File:** `src/pipeline/executor.rs:1177-1186`
- **Attack scenario:** Pipeline registry token inserted WITHOUT `registry_tag_pattern`. Any pipeline step can push images to ANY repository in the registry, overwriting other projects' production images.
- **Impact:** Cross-project supply chain attack via image replacement.
- **Remediation:** Add `registry_tag_pattern = "{project_name}/*"` to the pipeline registry token INSERT.
- **Found by:** Agent 9

### S17: [HIGH] receive-pack collects entire push body into memory (OOM) — *also A19*
- **Category:** A04: Insecure Design
- **Component:** Git HTTP
- **File:** `src/git/smart_http.rs:460-466`
- **Attack scenario:** Send large pack files (up to 500 MB limit). `body.collect().await.to_bytes()` loads entire payload into contiguous memory. Multiple concurrent pushes compound the issue.
- **Impact:** Server OOM / denial of service.
- **Remediation:** Stream body to git stdin. Parse pkt-line commands from first few KB for branch protection, then stream remainder.
- **Found by:** Agent 7

### S18: [HIGH] Registry blob upload reassembles all chunks into memory — *also A21, A83*
- **Category:** A04: Insecure Design
- **Component:** OCI Registry
- **File:** `src/registry/blobs.rs:263-273`
- **Attack scenario:** Upload many large chunks, then complete upload. `complete_upload` reads ALL parts from MinIO into `full_data` in memory.
- **Impact:** Server OOM from large blob finalization. Multiple concurrent completions exhaust memory.
- **Remediation:** Stream parts directly to final MinIO path or compute digest in streaming fashion.
- **Found by:** Agent 7

### S19: [HIGH] Deployer applies manifests without pod spec validation
- **Category:** A01: Broken Access Control
- **Component:** Deployer
- **File:** `src/deployer/applier.rs:32-52`
- **Attack scenario:** `ALLOWED_KINDS` includes Deployment, DaemonSet, StatefulSet. Deployer forces namespace but does NOT validate pod specs. A malicious ops-repo manifest defines a Deployment with `privileged: true`, `hostNetwork: true`, or `hostPath` mounts.
- **Impact:** Full node compromise via privileged container in a workload manifest.
- **Remediation:** Validate pod specs before apply: reject `hostNetwork`, `hostPID`, `privileged`, `hostPath`. Apply PodSecurity Standards on managed namespaces.
- **Found by:** Agent 6

### S20: [HIGH] Workspace owner demotion via add_member upsert
- **Category:** A01: Broken Access Control
- **Component:** RBAC
- **File:** `src/api/workspaces.rs:306-341`
- **Attack scenario:** A workspace admin calls `add_member` with the workspace owner's user_id and `role="member"`. The `ON CONFLICT ... DO UPDATE SET role` upsert overwrites the owner's role, demoting them to member.
- **Impact:** Workspace admin seizes control by demoting the owner. Owner loses admin/owner-derived permissions on all workspace projects.
- **Remediation:** Prevent modifying existing members who have "owner" role.
- **Found by:** Agent 2

### S21: [HIGH] Observe queries without project_id return all projects' data
- **Category:** A01: Broken Access Control
- **Component:** Observability
- **File:** `src/observe/query.rs:332-337`
- **Attack scenario:** User with ObserveRead calls `GET /api/observe/logs` without `project_id`. SQL uses `($1::uuid IS NULL OR project_id = $1)` — NULL returns ALL projects' data. The `require_observe_read()` function only checks project access when `project_id` is Some.
- **Impact:** Any user with ObserveRead on one project can see logs, traces, and metrics from every project.
- **Remediation:** When `project_id` is None, require admin or iterate over permitted projects.
- **Found by:** Agent 10

### S22: [HIGH] set_role_permissions does not invalidate permission caches — *also A26*
- **Category:** A01: Broken Access Control
- **Component:** RBAC
- **File:** `src/api/admin.rs:243-299`
- **Attack scenario:** Admin removes a permission from a role. Users with that role retain old permissions in Valkey cache for up to 5 minutes (default TTL). Attacker who knows revocation is imminent has a window to perform privileged operations.
- **Impact:** 5-minute permission revocation delay.
- **Remediation:** Query users holding the role and call `invalidate_permissions()` for each.
- **Found by:** Agent 2

### S23: [HIGH] Pipeline pods bypass NetworkPolicy (wrong podSelector)
- **Category:** A05: Security Misconfiguration
- **Component:** K8s networking
- **File:** `src/deployer/namespace.rs:354-415`
- **Attack scenario:** NetworkPolicy uses `podSelector: matchLabels: platform.io/component: agent-session`. Pipeline pods have different labels (`platform.io/pipeline`) and are NOT covered. Pipeline pods have unrestricted network access — can reach Postgres, Valkey, MinIO, other namespaces, K8s API.
- **Impact:** Malicious `.platform.yaml` pipeline exfiltrates data from infrastructure services.
- **Remediation:** Create separate NetworkPolicy for pipeline pods, or use default-deny with `podSelector: {}`.
- **Found by:** Agent 5, Agent 6

### S24: [HIGH] No default-deny NetworkPolicy in project namespaces
- **Category:** A05: Security Misconfiguration
- **Component:** K8s networking
- **File:** `src/deployer/namespace.rs:354-415`
- **Attack scenario:** Only agent-session-labeled pods have NetworkPolicy. Any pod without the label (pipeline pods, user-deployed workloads via deployer) has NO restrictions.
- **Impact:** User-deployed workloads in `-dev` and `-prod` namespaces communicate freely with all cluster services.
- **Remediation:** Add default-deny NetworkPolicy with `podSelector: {}`, then add specific allow policies.
- **Found by:** Agent 5

### S25: [HIGH] trustProxy defaults to true in Helm without reverse proxy
- **Category:** A07: Authentication Failures
- **Component:** Configuration
- **File:** `helm/platform/values.yaml:29`
- **Attack scenario:** Default Helm values set `trustProxy: true`. Default service type is NodePort (no proxy). Any client sets `X-Forwarded-For` to spoof IP address. Rate limiting bypass, IP-based audit trail unreliable.
- **Impact:** Rate limit bypass. Attacker spoofs IPs to frame others in audit logs.
- **Remediation:** Default to `trustProxy: false`. Tie to `ingress.enabled`.
- **Found by:** Agent 5

### S26: [HIGH] Platform container runs as root with no securityContext — *also E7*
- **Category:** A05: Security Misconfiguration
- **Component:** Container security
- **Files:** `docker/Dockerfile:43-54`, `helm/platform/templates/deployment.yaml:58-103`
- **Attack scenario:** Platform Dockerfile has no USER directive. Helm deployment has zero securityContext settings. Container runs as root with full capabilities and the platform's broad ClusterRole.
- **Impact:** RCE in the platform binary gives root + all capabilities + ClusterRole with cluster-wide secrets access.
- **Remediation:** Add non-root USER to Dockerfile. Add `runAsNonRoot: true`, `readOnlyRootFilesystem: true`, `capabilities: drop: ["ALL"]` to Helm deployment.
- **Found by:** Agent 6

### S27: [HIGH] Unpinned npm install of claude-code CLI in Docker — *also E9*
- **Category:** A06: Vulnerable Components
- **Component:** Docker images
- **File:** `docker/Dockerfile.platform-runner:48`
- **Attack scenario:** `npm install -g @anthropic-ai/claude-code` installs whatever "latest" is. Compromised npm account delivers malicious code into every agent pod.
- **Impact:** Code execution in agent containers with access to API tokens and project code.
- **Remediation:** Pin to exact version: `npm install -g @anthropic-ai/claude-code@X.Y.Z`
- **Found by:** Agent 8

### S28: [HIGH] kaniko executor pinned to :latest tag — *also E9*
- **Category:** A06: Vulnerable Components
- **Component:** Docker images
- **File:** `docker/Dockerfile.platform-runner:22`
- **Attack scenario:** `gcr.io/kaniko-project/executor:latest` is mutable. Compromised kaniko release delivers trojanized executor. Kaniko runs with root-equivalent privileges.
- **Impact:** Arbitrary code execution during container builds inside agent pods.
- **Remediation:** Pin to digest: `gcr.io/kaniko-project/executor@sha256:<digest>`
- **Found by:** Agent 8

### S29: [HIGH] API token written to plaintext file on disk in agent pods — *also E17*
- **Category:** A04: Insecure Design
- **Component:** Agent runtime
- **File:** `docker/entrypoint.sh:89`
- **Attack scenario:** `PLATFORM_API_TOKEN` written to `/workspace/.platform/.env` in cleartext. Any process in the agent container (including untrusted cloned code) reads this file.
- **Impact:** Token exfiltration allows impersonating the agent session.
- **Remediation:** Pass token via environment variable only (not files). Or use tmpfs mount.
- **Found by:** Agent 8

### S30: [HIGH] ClusterRole grants namespace create/delete and RBAC write cluster-wide
- **Category:** A01: Broken Access Control
- **Component:** K8s RBAC
- **File:** `helm/platform/templates/clusterrole.yaml:8-11, 57-60`
- **Attack scenario:** Platform can create/delete any namespace, create Roles/RoleBindings in any namespace. Compromised platform could delete `kube-system` or grant itself admin in any namespace.
- **Impact:** Cluster-wide denial of service or privilege escalation.
- **Remediation:** Use label selectors. Add admission control to restrict to `platform.io/managed-by: platform` labeled namespaces.
- **Found by:** Agent 6

### S31: [HIGH] Git auth token exposed as env var in pipeline init containers
- **Category:** A09: Logging & Monitoring Failures
- **Component:** Pipeline sandbox
- **File:** `src/pipeline/executor.rs:1354-1364`
- **Attack scenario:** `GIT_AUTH_TOKEN` passed as plain env var, visible in pod spec. Anyone with kubectl access or the step container itself reads it via `/proc/1/environ`.
- **Impact:** 1-hour window to clone/push to project repo.
- **Remediation:** Mount via K8s Secret volume. Delete Secret after clone completes.
- **Found by:** Agent 9

### S32: [HIGH] Passkey login loads ALL credentials into memory (DoS) — *also A23*
- **Category:** A07: Authentication Failures
- **Component:** Authentication
- **File:** `src/api/passkeys.rs:346-358`
- **Attack scenario:** Every passkey login queries ALL passkey credentials for ALL active users (no LIMIT). Repeated calls exhaust memory as user base grows.
- **Impact:** Denial of service via memory exhaustion.
- **Remediation:** Add LIMIT clause, cache credential list with short TTL, rate-limit `begin_login` endpoint.
- **Found by:** Agent 1

### S33: [HIGH] Host path mount in agent pods not gated on dev_mode
- **Category:** A04: Insecure Design
- **Component:** Agent sandbox
- **File:** `src/agent/claude_code/pod.rs:181-198`
- **Attack scenario:** When `PLATFORM_HOST_MOUNT_PATH` is set, a host directory is mounted into agent pods. If this env var leaks or is set in production, agent pods get host filesystem access.
- **Impact:** Host filesystem access, potentially including kubelet credentials.
- **Remediation:** Gate on explicit `dev_mode` check. Panic if host mounts configured in non-dev mode.
- **Found by:** Agent 9

### S34: [HIGH] MCP npm install without --ignore-scripts
- **Category:** A06: Vulnerable Components
- **Component:** Docker images
- **File:** `docker/Dockerfile.platform-runner:52`
- **Attack scenario:** `npm install --production` runs lifecycle scripts of all transitive dependencies. Malicious transitive dep executes arbitrary code during build.
- **Impact:** Code injection into agent runner image.
- **Remediation:** Use `npm ci --ignore-scripts --production`.
- **Found by:** Agent 8

### S35: [HIGH] No cargo-audit in CI pipeline
- **Category:** A06: Vulnerable Components
- **Component:** CI/CD
- **File:** `.github/workflows/ci.yaml`
- **Attack scenario:** Known CVEs in Rust dependencies go undetected. 3 advisories currently suppressed in deny.toml including RSA timing side-channel (RUSTSEC-2023-0071) which affects the SSH server.
- **Impact:** Vulnerable dependencies ship to production.
- **Remediation:** Add `cargo audit` CI step. Re-evaluate suppressed advisories.
- **Found by:** Agent 8

---

## Medium Findings (fix within current sprint)

### S36: [MEDIUM] Password change doesn't require current password — *also E28*
- **File:** `src/api/users.rs:500-503`
- **Attack scenario:** PATCH /api/users/{id} accepts new password without requiring current password. Session hijack → permanent account takeover.
- **Remediation:** Require `current_password` field for self-service password changes.

### S37: [MEDIUM] Login rate limit keyed on username only — enables account lockout — *also A65*
- **File:** `src/api/users.rs:148`
- **Attack scenario:** Attacker sends 11 login attempts with a target username within 5 minutes. Legitimate user locked out. Password spraying across users also undetected.
- **Remediation:** Add secondary rate limit keyed on client IP.

### S38: [MEDIUM] Rate limit INCR/EXPIRE race condition — *also A64*
- **File:** `src/auth/rate_limit.rs:19-27`
- **Attack scenario:** If EXPIRE fails after INCR, key persists forever without TTL. Permanent lockout.
- **Remediation:** Always set EXPIRE regardless of count (idempotent).

### S39: [MEDIUM] begin_login passkey endpoint has no rate limiting
- **File:** `src/api/passkeys.rs:318`
- **Attack scenario:** Flood unauthenticated POST to generate Valkey challenge objects (120s TTL each). Memory exhaustion.
- **Remediation:** Add `check_rate()` keyed on client IP.

### S40: [MEDIUM] Config struct derives Debug — secrets in logs on panic — *also A16*
- **File:** `src/config.rs:4`
- **Attack scenario:** If Config is logged or appears in a panic backtrace, all secrets (master_key, database_url, smtp_password, etc.) are printed.
- **Remediation:** Custom Debug impl that redacts sensitive fields.

### S41: [MEDIUM] No zeroize on decrypted secret material in memory — *also A85*
- **File:** `src/secrets/engine.rs:38`
- **Attack scenario:** Decrypted secrets live on heap until allocator reuses the page. Memory dump/core dump recovers plaintext.
- **Remediation:** Use `Zeroizing<Vec<u8>>` from zeroize crate.

### S42: [MEDIUM] Webhook URLs logged (may contain tokens) — *also A25, A58*
- **Files:** `src/notify/webhook.rs:63,70`, `src/api/webhooks.rs:473,480,501,504`
- **Attack scenario:** Slack webhook URLs contain tokens in path. Log readers extract tokens.
- **Remediation:** Log scheme+host only, or webhook ID.

### S43: [MEDIUM] Secret read endpoint returns full plaintext without audit
- **File:** `src/api/secrets.rs:306`
- **Attack scenario:** Any user with SecretRead on a project retrieves full decrypted value. Reads not audited.
- **Remediation:** Audit-log every secret read. Consider rate limiting and confirmation header.

### S44: [MEDIUM] No master key rotation mechanism
- **File:** `src/secrets/engine.rs`
- **Attack scenario:** Key compromise is permanent — no re-encryption mechanism. No key_version in encrypted blob.
- **Remediation:** Add key_version field. Implement background re-encryption migration.

### S45: [MEDIUM] No workspace scope enforcement on workspace endpoints
- **File:** `src/api/workspaces.rs`
- **Attack scenario:** API token with `boundary_workspace_id=A` can access workspace B. `check_workspace_scope()` exists but is never called.
- **Remediation:** Add `auth.check_workspace_scope(id)?` to all workspace handlers.

### S46: [MEDIUM] Transitive delegation chain creates uncontrolled privilege expansion — *also A6*
- **File:** `src/rbac/delegation.rs:37-106`
- **Attack scenario:** Admin delegates `admin:delegate` to B. B re-delegates to C. C to D. Unlimited chain.
- **Remediation:** Exclude delegated permissions from the "delegator holds this" check, or enforce max chain depth.

### S47: [MEDIUM] Any user with admin:delegate can revoke ANY delegation
- **File:** `src/api/admin.rs:437-462`
- **Attack scenario:** User with `admin:delegate` revokes delegations created by other admins.
- **Remediation:** Verify `auth.user_id == delegation.delegator_id` before allowing revocation.

### S48: [MEDIUM] Dev mode predictable credentials/master key
- **Files:** `src/main.rs:84`, `src/store/bootstrap.rs:317`
- **Attack scenario:** `PLATFORM_DEV=true` → admin password "admin", master key all-zeros. Accidentally enabled in prod = full compromise.
- **Remediation:** Generate random password/key even in dev mode. Add prominent warning.

### S49: [MEDIUM] Content-Disposition header injection in release assets — *also A46*
- **File:** `src/api/releases.rs:477`
- **Attack scenario:** Release asset name containing `"\r\n` injects headers. HTTP response splitting / cache poisoning.
- **Remediation:** Use `sanitize_filename()` from `src/api/pipelines.rs`.

### S50: [MEDIUM] Missing Content-Security-Policy header — *also A8*
- **File:** `src/main.rs:246-259`
- **Attack scenario:** No CSP → XSS can inject and execute arbitrary scripts.
- **Remediation:** Add CSP: `default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; frame-src 'none'; object-src 'none'`

### S51: [MEDIUM] Missing Strict-Transport-Security (HSTS) header
- **File:** `src/main.rs:246-259`
- **Attack scenario:** Without HSTS, SSL stripping attack possible on first visit. Session cookies sent over plaintext.
- **Remediation:** Add HSTS when TLS configured.

### S52: [MEDIUM] No rate limiting on Git HTTP Basic Auth
- **File:** `src/git/smart_http.rs:113-215`
- **Attack scenario:** Unlimited password/token guessing via git clone/push. API tokens (SHA-256) fast to verify.
- **Remediation:** Add `check_rate` in `authenticate_basic` or `check_access`.

### S53: [MEDIUM] No rate limiting on registry auth
- **File:** `src/registry/auth.rs:59-113`
- **Attack scenario:** Unlimited token guessing via docker pull/push.
- **Remediation:** Add rate limiting to `RegistryUser` extractor.

### S54: [MEDIUM] OCI registry tags are mutable (supply chain risk)
- **File:** `src/registry/manifests.rs:129-141`
- **Attack scenario:** Push new manifest with same tag (e.g., `v1.0.0`). ON CONFLICT overwrites existing tag.
- **Impact:** Silent image replacement — supply chain attack.
- **Remediation:** Add immutable tag policy option per repository.

### S55: [MEDIUM] MinIO connection uses HTTP, not HTTPS
- **File:** `src/main.rs:120-128`
- **Attack scenario:** Default `http://localhost:9000`. Object storage traffic (Parquet, OCI blobs, LFS) unencrypted.
- **Remediation:** Use HTTPS endpoint for MinIO in production.

### S56: [MEDIUM] Helm master key generated with randAlphaNum, not hex — *also E3a*
- **File:** `helm/platform/templates/secret.yaml:50`
- **Attack scenario:** `randAlphaNum 64` produces chars outside hex range. `parse_master_key()` expects hex. Fresh installs may fail or have reduced entropy.
- **Remediation:** Use `{{ randBytes 32 | printf "%x" }}`.

### S57: [MEDIUM] No prod namespace NetworkPolicy
- **File:** `src/api/projects.rs:224-236`
- **Attack scenario:** Deployed workloads in prod namespaces have unrestricted network access.
- **Remediation:** Apply default-deny NetworkPolicy to prod namespaces.

### S58: [MEDIUM] Data store NetworkPolicy allows all pods in platform namespace
- **File:** `helm/platform/templates/networkpolicy-data.yaml:9-24`
- **Attack scenario:** Any pod in the platform namespace (debug pods, sidecars) gets full Postgres/Valkey/MinIO access.
- **Remediation:** Restrict to platform pod's selector labels only.

### S59: [MEDIUM] Proxy trust is all-or-nothing (no CIDR restriction)
- **File:** `src/auth/middleware.rs:243-257`
- **Attack scenario:** When trust_proxy is true, X-Forwarded-For trusted from ANY source IP.
- **Remediation:** Add `PLATFORM_TRUST_PROXY_CIDR` for trusted proxy IP range.

### S60: [MEDIUM] Pipeline images not validated with check_container_image() — *also A7*
- **File:** `src/pipeline/definition.rs:426`
- **Attack scenario:** Step image names not checked for shell metacharacters at definition validation time.
- **Remediation:** Call `check_container_image()` on step.image during `validate()`.

### S61: [MEDIUM] Git merge errors leak stderr to API response
- **File:** `src/api/merge_requests.rs:1055,1058,1061`
- **Attack scenario:** Git stderr (file paths, version info) returned verbatim via BadRequest.
- **Remediation:** Return generic "merge failed"; log details server-side.

### S62: [MEDIUM] Observe queries cross-project data (no project_id = all data)
- **File:** `src/observe/query.rs:332-337`
- Duplicate of S21 context. Covered under S21.

### S63: [MEDIUM] Dispatch_single does not re-validate webhook URL (latent SSRF)
- **File:** `src/api/webhooks.rs:470`
- **Attack scenario:** If webhook URL modified directly in DB, dispatch fetches without re-checking SSRF.
- **Remediation:** Add SSRF re-validation before HTTP request.

### S64: [MEDIUM] NodePort service created as fallback when ingress disabled
- **File:** `helm/platform/templates/service-nodeport.yaml`
- **Attack scenario:** Port 8080 exposed on every node (30000-32767 range). Accessible to anyone reaching any node IP.
- **Remediation:** Make NodePort opt-in. ClusterIP should be default when ingress disabled.

### S65: [MEDIUM] Agent token 24-hour expiry is too long
- **File:** `src/agent/identity.rs:98`
- **Attack scenario:** Sessions complete in minutes but token valid 24 hours. Exfiltrated token usable long after session ends.
- **Remediation:** Reduce to 2 hours. Ensure cleanup retries on failure.

### S66: [MEDIUM] Known HIGH npm vulnerabilities in MCP dependencies
- **File:** `mcp/package.json`
- **Attack scenario:** hono (cookie injection, SSE injection, arbitrary file access, prototype pollution), express-rate-limit (IPv4-mapped IPv6 bypass). MCP servers run in agent pods with API tokens.
- **Remediation:** Update deps. Add overrides for patched versions.

### S67: [MEDIUM] MCP package ecosystem not covered by Dependabot — *also E57*
- **File:** `.github/dependabot.yml`
- **Remediation:** Add npm entry for `/mcp` directory.

### S68: [MEDIUM] Suppressed Rust advisory RUSTSEC-2023-0071 affects SSH server
- **File:** `deny.toml:3-12`
- **Attack scenario:** RSA Marvin Attack timing side-channel in russh's network-facing SSH server.
- **Remediation:** Re-evaluate exploitability. Check if russh 0.50+ fixes this.

---

## Low Findings (defense-in-depth)

- [LOW] S69: `src/api/users.rs:275` — Logout deletes ALL sessions (no single-device logout) — *also A24*
- [LOW] S70: `src/auth/password.rs:20` — Argon2 default params (19 MiB) below OWASP minimum (46 MiB)
- [LOW] S71: `src/api/users.rs:629` — Token expiry max 365 days (consider 90-180 max)
- [LOW] S72: `src/auth/middleware.rs:286` — last_used_at update fire-and-forget (stale metadata)
- [LOW] S73: `src/api/passkeys.rs:392` — Platform authenticator clone detection edge case (counter=0)
- [LOW] S74: `src/api/projects.rs:494` — ILIKE search allows wildcard abuse (`%`, `_`) — *also A53*
- [LOW] S75: `src/pipeline/trigger.rs:301` — on_api() does not validate git_ref — *also A73*
- [LOW] S76: No server-side markdown HTML stripping (frontend XSS depends on sanitizer) — *see also E1 (stored XSS)*
- [LOW] S77: DNS rebinding not fully mitigated in webhook SSRF validation
- [LOW] S78: Pipeline images — any registry allowed (no allowlist)
- [LOW] S79: `src/validation.rs:22` — check_name() uses Unicode is_alphanumeric (not ASCII) — *also A12*
- [LOW] S80: `src/validation.rs:215` — check_setup_commands() unrestricted content (by design)
- [LOW] S81: `src/notify/email.rs:42` — SMTP starttls_relay vulnerable to downgrade
- [LOW] S82: `src/main.rs:246` — Missing Permissions-Policy header
- [LOW] S83: CORS WebSocket upgrade may bypass CORS check (auth still required)
- [LOW] S84: `src/pipeline/executor.rs:1757` — Test namespace has no NetworkPolicy
- [LOW] S85: `hack/cluster-up.sh:50` — Gateway allows routes from ALL namespaces
- [LOW] S86: `helm/platform/values.yaml:28` — secureCookies defaults to false
- [LOW] S87: deny.toml openssl ban has webauthn-rs exception (tracking 6.0 release)
- [LOW] S88: `src/git/protection.rs:96` — Force push detection fails open on git error — *also A82*
- [LOW] S89: `src/registry/mod.rs:99` — expect() panic on NULL workspace_id
- [LOW] S90: `src/git/lfs.rs:30` — LFS object size field not validated (signed i64) — *also A20*
- [LOW] S91: `src/git/lfs.rs:135` — No limit on LFS batch object count — *also A20*
- [LOW] S92: `src/registry/blobs.rs:80` — Blob served from MinIO into memory (no streaming) — *also A21*
- [LOW] S93: `src/git/hooks.rs:331` — Tag name passed without validation to git rev-parse
- [LOW] S94: `src/observe/store.rs` — No data retention/purging for observability tables
- [LOW] S95: `ui/src/pages/admin/Health.tsx:139` — Token in localStorage (XSS risk) — *also E44*
- [LOW] S96: `docker/Dockerfile.platform-runner:57` — Passwordless sudo for agent user (see S4, also E8)

---

## Component Security Summary

### Authentication & Sessions — ACCEPTABLE
Strong fundamentals: timing-safe login, proper token hashing, setup endpoint gated, deactivation kills all sessions. Gaps: rate limit keyed on username only (lockout), no current-password requirement for password change, passkey DoS at scale.

### Authorization & RBAC — NEEDS HARDENING
Good permission model with intersection semantics and cache. Critical gap: deleted workspace permissions persist permanently. Role permission cache not invalidated on change. Workspace scope boundaries not enforced on workspace endpoints. Transitive delegation chains uncontrolled.

### Input Validation & Injection — STRONG
All SQL parameterized. Git subprocess args use OS-level separation. Path traversal blocked at multiple layers. SSRF protection on webhooks. Minor gaps: pipeline image validation at definition time, header injection in release assets.

### Secrets & Cryptography — ACCEPTABLE
Proper AES-256-GCM with CSPRNG nonces. SHA-256 token hashing. No nonce reuse. Gaps: no key rotation mechanism, no zeroize on sensitive memory, Config derives Debug with secrets, webhook URLs logged.

### Network & Transport — NEEDS HARDENING
Critical gaps: Postgres/Valkey connections plaintext, no TLS/auth. MinIO HTTP. NetworkPolicy only covers agent-session pods — pipeline pods and deployed workloads unrestricted. trustProxy defaults to true without proxy. No CSP/HSTS headers.

### Container & K8s — CRITICAL GAPS
Platform runs as root with no securityContext. ClusterRole grants cluster-wide secrets and RBAC write. Agent pods allow privilege escalation with sudo. Pipeline pods run as root. No PodSecurityAdmission on managed namespaces. Deployer applies unvalidated manifests.

### Agent & Pipeline Sandbox — NEEDS HARDENING
Good isolation patterns (per-session namespace, Valkey ACL, env var isolation, permission intersection). Critical gaps: agent can create privileged pods, agent-edit Role grants secrets access, privilege escalation enabled, API token on disk.

### Supply Chain — CRITICAL GAPS
GitHub Actions not SHA-pinned (proven attack vector). kubectl/kaniko downloaded without verification. curl|bash in Dockerfile. NPM installs unpinned. No cargo-audit in CI. MCP dependencies with known HIGH vulns not in Dependabot.

### Information Disclosure — NEEDS HARDENING
Internal errors properly sanitized (generic 500). No Server header. Good: webhook URLs not logged (but actually they are in dispatch_single). Critical gap: audit log endpoint has no authorization — any user sees all platform mutations. Observe queries leak cross-project data.

## Trust Boundary Diagram

```
[External Users]
    │
    ├── HTTPS ──▶ [Ingress / Load Balancer] (optional — NodePort fallback)
    │                  │
    │                  ├──▶ [Platform API :8080] ◄── AuthUser + RBAC
    │                  │       │  ⚠ Runs as ROOT (S26)
    │                  │       │  ⚠ ClusterRole: cluster-wide secrets (S6)
    │                  │       │
    │                  │       ├── Postgres ⚠ PLAINTEXT (S12)
    │                  │       ├── Valkey ⚠ PLAINTEXT, NO AUTH (S13)
    │                  │       ├── MinIO ⚠ HTTP (S55)
    │                  │       └── K8s API (broad ClusterRole)
    │                  │
    │                  ├──▶ [Git SSH :2222] ◄── SSH key auth
    │                  │       ⚠ BYPASSES branch protection (S1)
    │                  │
    │                  └──▶ [Web UI] ◄── Session cookie
    │
    ├── [Agent Pods] ◄── Scoped token + Valkey ACL
    │       │  ⚠ allowPrivilegeEscalation + sudo (S4)
    │       │  ⚠ Can create privileged pods (S3)
    │       │  ⚠ Secrets access in namespace (S5)
    │       ├── Platform API (boundary-scoped)
    │       ├── Valkey (own channels only) ✓
    │       ├── MCP servers (local)
    │       └── Internet (egress allowed)
    │
    └── [Pipeline Pods]
            │  ⚠ Runs as ROOT (S15)
            │  ⚠ NO NetworkPolicy coverage (S23)
            │  ⚠ Registry token: no tag pattern (S16)
            ├── Registry (push — unrestricted tags)
            ├── Internet (egress)
            └── ⚠ Postgres/Valkey/MinIO reachable (S23)
```

## Recommended Remediation Plan

### Immediate — before external exposure
1. **S1, S2:** Fix SSH branch protection bypass — enforce `enforce_push_protection()` on SSH push path. Parse pushed refs.
2. **S3, S4:** Harden agent sandbox — PodSecurityAdmission `baseline` on session namespaces, drop capabilities on agent main container, ResourceQuota + LimitRange per session namespace. (S5 accepted — see DD-2.)
3. **S6, S30:** Scope ClusterRole secrets — split into shared ClusterRole template + per-namespace RoleBinding (Option C). Remove `secrets` from the ClusterRoleBinding. Keep `roles`/`rolebindings` cluster-wide for bootstrap.
4. **S7, S10:** Pin all GitHub Actions to SHA digests. Add `permissions: {}` to ci.yaml.
5. **S14:** Add `require_admin()` to `list_audit_log` endpoint.
6. **S11:** Fix workspace permissions query to check `workspaces.is_active = true`.

### Short-term — within 2 weeks
7. **S12:** Postgres TLS everywhere — self-signed init container in dev postgres manifest, `?sslmode=require` in test/Helm DATABASE_URL, Bitnami `tls.enabled: true` in values.yaml. Zero code changes needed (sqlx reads sslmode from URL).
8. **S13, S55:** Valkey auth + TLS, MinIO HTTPS.
8. **S15, S16:** Pipeline pod hardening — securityContext for non-kaniko steps, add registry_tag_pattern.
9. **S23, S24:** Default-deny NetworkPolicy in all managed namespaces.
10. **S8, S9, S27, S28, S34:** Docker supply chain — pin kubectl with checksum, pin kaniko digest, pin npm versions, use --ignore-scripts.
11. **S19:** Deployer manifest validation — reject privileged pods, hostPath, hostNetwork.
12. **S26:** Non-root platform container with proper securityContext.
13. **S25:** Default trustProxy to false.

### Medium-term — within 1 month
14. **S22:** Cache invalidation on role permission changes.
15. **S36, S37, S38, S39:** Auth hardening — require current password, dual rate limit keys, fix race condition, rate-limit passkey begin.
16. **S40, S41, S42:** Secrets hygiene — custom Debug on Config, zeroize, redact webhook URLs in logs.
17. **S50, S51:** Security headers — add CSP, HSTS.
18. **S43, S44:** Secret read audit logging, key rotation mechanism.
19. **S45, S46, S47:** Workspace scope enforcement, delegation chain limits.
20. **S52, S53:** Rate limiting on git HTTP auth and registry auth.

### Ongoing
21. Install and run `cargo audit` + `npm audit` in CI.
22. Add `/mcp` to Dependabot.
23. Periodic re-audit schedule (quarterly).
24. Implement observability data retention/purging.
25. Track webauthn-rs 6.0 for openssl removal.
