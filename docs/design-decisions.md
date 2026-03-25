# Design Decisions & Accepted Trade-offs

This document records deliberate design choices where the platform accepts a known trade-off. Each entry references the audit finding that flagged it, explains the reasoning, and describes the mitigations that make the trade-off acceptable.

---

## DD-1: Agent containers have passwordless sudo

**Audit ref:** E8 — `docker/Dockerfile.platform-runner:57` — `NOPASSWD: ALL`

**Decision:** Agent runner containers grant the `agent` user (UID 1000) passwordless sudo.

**Why:** Agents must be able to install arbitrary tooling (compilers, language runtimes, system libraries, CLI tools) to complete the user's task. The task is open-ended — a user might ask an agent to build a Rust project (needs `cargo`), process images (needs `imagemagick`), or deploy infrastructure (needs `terraform`). Restricting sudo to a fixed allowlist would cripple the agent's ability to do useful work.

**Threat model:**

| Threat | Mitigation |
|---|---|
| Agent escalates to root inside container | Accepted. The container is ephemeral and single-tenant — root inside the container has no access beyond it. |
| Root in container escapes to host | Containers run without `--privileged`. No host PID/network namespace. K8s `securityContext` does not grant `SYS_ADMIN` or other dangerous capabilities. Container runtime (containerd) enforces isolation. |
| Agent accesses other pods/services | NetworkPolicy restricts egress to only the platform API, Valkey (scoped ACL), and the public internet. No access to Postgres, MinIO, K8s API, or other tenant namespaces. |
| Agent consumes excessive resources | Resource limits (CPU, memory) enforced via pod spec. Session idle timeout kills abandoned pods. |
| Malicious package installed via sudo | The user's own Claude API key is used. The user is responsible for the prompts they send and the actions the agent takes — same as running Claude Code locally with `--dangerously-skip-permissions`. |
| Agent persists across sessions | Containers are ephemeral. Pod is deleted on session termination. No persistent volumes survive beyond the session (workspace is committed to git or lost). |

**Context:** The platform is designed to be self-hosted. The operator and user are typically the same person or team. The agent runs with the user's own LLM API key and operates under the user's authority — comparable to giving a developer a VM with sudo. The isolation boundary is the container + namespace + network policy, not the UID inside the container.

**Alternatives considered:**

1. **Restrict sudo to a fixed command allowlist** (`apt-get`, `npm`, `pip`). Rejected: too brittle. Agents regularly need `make`, `cmake`, system-level config changes, service restarts during testing, etc. Every missing command becomes a support issue.

2. **Remove sudo entirely, pre-install all tooling.** Rejected: impossible to predict what every project needs. Would require per-project custom images, defeating the purpose of a general-purpose agent.

3. **Use rootless containers (user namespaces).** Worth revisiting when K8s user namespace support matures. Currently adds complexity without meaningful security gain given the existing isolation layers.

**Review trigger:** Revisit if the platform adds multi-tenant shared clusters where different users' agents run on the same nodes without trust, or if K8s user namespace support becomes stable.

---

## DD-2: Agent RBAC Role grants full secrets access in session namespace

**Audit ref:** S5 — `src/deployer/namespace.rs:43-63` — `agent-edit` Role grants `verbs: ["*"]` on `secrets`

**Decision:** The `agent-edit` Role gives the agent full CRUD on Kubernetes Secrets within its own session namespace.

**Why:** Agents need to create, read, update, and delete Secrets as part of normal development workflows. Common use cases:

- Creating Secrets for applications the agent deploys in its session namespace (database credentials, API keys for the app under development)
- Reading Secrets to debug failing deployments (`kubectl describe secret`, `kubectl get secret -o yaml`)
- Updating Secrets when rotating credentials or fixing configuration during iterative development
- Deleting Secrets when cleaning up failed deployments or starting fresh

Restricting Secrets access would break the agent's ability to do standard Kubernetes development work — the same operations a developer performs daily with `kubectl`.

**Threat model:**

| Threat | Mitigation |
|---|---|
| Agent reads the registry push secret (contains scoped API token) | The registry token is boundary-scoped to the project and has a `registry_tag_pattern` restricting which image tags can be pushed. Even if exfiltrated, the token can only push images matching `{project}/session-{id}-*`. Token expires with the session. |
| Agent exfiltrates Secrets to external service | NetworkPolicy restricts egress. Agent API token is project-scoped. The user's own LLM API key drives the agent — the user is responsible for prompts. Same trust model as DD-1. |
| Agent creates Secrets to store malicious data | Session namespace is ephemeral. All resources (including Secrets) are deleted when the session terminates. No persistence beyond the session. |
| Agent reads Secrets from other namespaces | RBAC is namespace-scoped via a Role (not ClusterRole). The agent can only access Secrets in its own session namespace. Cross-namespace access is impossible via RBAC. |

**Context:** Each agent session gets its own dedicated K8s namespace (`{project-slug}-dev-session-{id}`). The namespace is single-tenant — only one agent operates in it. The entire namespace is deleted on session termination. The agent's ServiceAccount token is scoped to this namespace only via a RoleBinding. This is equivalent to giving a developer `kubectl` access to their own dev namespace.

**Alternatives considered:**

1. **Restrict to specific `resourceNames`.** Rejected: the agent creates Secrets with dynamic names during development. A fixed allowlist would prevent the agent from managing application Secrets.

2. **Remove Secrets from the Role, provide a platform API for secret management.** Rejected: adds unnecessary indirection. The agent already has a platform API token for platform-level secrets (`/api/projects/{id}/secrets`). K8s-level Secrets are for the agent's own namespace workloads — different use case, different scope.

3. **Grant read-only (no create/update/delete).** Rejected: agents need to create Secrets for the apps they deploy and delete them when cleaning up.

**Review trigger:** Revisit if session namespaces become shared between multiple agents or users, or if the registry push secret scope (tag pattern) is widened beyond session-specific tags.
