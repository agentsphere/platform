# 09 — Secrets Engine & Notifications

## Prerequisite
- 01-foundation complete (store, AppState)
- 02-identity-auth complete (AuthUser, RequirePermission)

## Blocks
- Nothing — self-contained modules

## Can Parallelize With
- 03-git-server, 04-project-mgmt, 05-build-engine, 06-deployer, 07-agent, 08-observability

---

## Scope

Two small, independent modules bundled into one plan:

1. **Secrets engine** — AES-256-GCM encrypted secret storage in Postgres, scoped by project and usage (pipeline, agent, deploy). Replaces OpenBao.
2. **Notifications** — multi-channel dispatch (in-app, email via SMTP, webhook). Replaces Maddy for notification purposes.

---

## Part A: Secrets Engine (~500 LOC)

### 1. `src/secrets/mod.rs` — Module Root
Re-exports engine.

### 2. `src/secrets/engine.rs` — Encrypt/Decrypt & CRUD

**Encryption**:
- `pub fn encrypt(plaintext: &[u8], master_key: &[u8; 32]) -> Result<Vec<u8>>`
  - Generate random 12-byte nonce
  - Encrypt with AES-256-GCM using `aes-gcm` crate
  - Return: `nonce || ciphertext || tag` (concatenated)
- `pub fn decrypt(encrypted: &[u8], master_key: &[u8; 32]) -> Result<Vec<u8>>`
  - Split nonce (first 12 bytes), ciphertext+tag (rest)
  - Decrypt and verify tag

**Master key**:
- Loaded from `PLATFORM_MASTER_KEY` env var (hex-encoded 32 bytes)
- In K8s: stored as a K8s Secret, mounted as env var
- Fail startup if not set in production mode

**Secret CRUD**:
- `pub async fn create_secret(pool, master_key, project_id, name, value, scope, created_by) -> Result<Secret>`
  - Encrypt value
  - Insert into `secrets` table
  - If exists: increment version, update encrypted_value
  - Audit log: `secret.create`
- `pub async fn delete_secret(pool, secret_id, actor_id) -> Result<()>`
  - Delete from table
  - Audit log: `secret.delete`
- `pub async fn list_secrets(pool, project_id) -> Result<Vec<SecretMetadata>>`
  - Returns: name, scope, version, created_at — NOT the value
- `pub async fn resolve_secret(pool, master_key, project_id, name, scope) -> Result<String>`
  - Decrypt and return plaintext
  - Only called internally by pipeline executor and agent identity
  - NOT exposed via API (secrets are write-only from API perspective)

**Secret resolution for pipelines/agents**:
- `pub async fn resolve_secrets_for_env(pool, master_key, project_id, scope, template: &str) -> Result<String>`
  - Replace `${{ secrets.NAME }}` patterns in pipeline configs or agent environment
  - Only resolves secrets matching the given scope

### 3. `src/api/secrets.rs` — Secrets API

- `POST /api/projects/:id/secrets` — create/update secret
  - Required: `name`, `value`, `scope` (pipeline/agent/deploy/all)
  - Value accepted in request body, encrypted before storage
  - Requires: `secret:write`
- `GET /api/projects/:id/secrets` — list secrets (metadata only)
  - Returns: name, scope, version, created_at — never the value
  - Requires: `secret:read`
- `DELETE /api/projects/:id/secrets/:name` — delete secret
  - Requires: `secret:write`

Global secrets (project_id = NULL):
- `POST /api/admin/secrets` — create global secret (admin only)
- `GET /api/admin/secrets` — list global secrets
- `DELETE /api/admin/secrets/:name` — delete global secret

---

## Part B: Notifications (~500 LOC)

### 1. `src/notify/mod.rs` — Module Root
Re-exports dispatch, email, webhook.

### 2. `src/notify/dispatch.rs` — Multi-channel Dispatcher

Central notification dispatch:

- `pub async fn notify(state: &AppState, notification: NewNotification) -> Result<()>`
  - Insert into `notifications` table (status: pending)
  - Based on `channel`:
    - `in_app` → just store in table (UI polls or WebSocket pushes)
    - `email` → call email::send()
    - `webhook` → call webhook::deliver()
  - Update status to `sent` or `failed`

- `NewNotification`:
  ```rust
  pub struct NewNotification {
      pub user_id: Uuid,
      pub notification_type: String,  // "build_failed", "mr_created", etc.
      pub subject: String,
      pub body: Option<String>,
      pub channel: NotifyChannel,
      pub ref_type: Option<String>,   // "pipeline", "mr", "session", "alert"
      pub ref_id: Option<Uuid>,
  }
  ```

- Event-driven helpers (called by other modules):
  - `pub async fn on_build_complete(state, pipeline) -> Result<()>` — notify project owner on build failure
  - `pub async fn on_mr_created(state, mr) -> Result<()>` — notify relevant reviewers
  - `pub async fn on_agent_completed(state, session) -> Result<()>` — notify session creator
  - `pub async fn on_deploy_status(state, deployment) -> Result<()>` — notify on deploy success/failure
  - `pub async fn on_alert_firing(state, alert_event) -> Result<()>` — notify alert subscribers

### 3. `src/notify/email.rs` — SMTP Email Client

- `pub async fn send(config: &SmtpConfig, to: &str, subject: &str, body: &str) -> Result<()>`
  - Use `lettre` crate with tokio transport
  - TLS via `rustls` (lettre feature)
  - Config: `smtp_host`, `smtp_port`, `smtp_from`, optional `smtp_username`/`smtp_password`
  - Simple text emails (no HTML templates for now — keep it lean)
  - Retry on transient failure (1 retry)

### 4. `src/notify/webhook.rs` — Webhook HTTP Delivery

- `pub async fn deliver(url: &str, payload: &serde_json::Value, secret: Option<&str>) -> Result<DeliveryResult>`
  - POST JSON payload to URL using `reqwest`
  - If `secret` provided: compute HMAC-SHA256 of body, include as `X-Platform-Signature` header
  - Timeout: 10 seconds
  - Return: status code, response body (for logging)
  - No retries (webhook delivery is best-effort)

### 5. `src/api/notifications.rs` — Notification API (for in-app)

- `GET /api/notifications` — list notifications for current user
  - Filter by: status (pending/sent/read), type
  - Paginated
  - Auth: current user only
- `PATCH /api/notifications/:id/read` — mark as read
- `GET /api/notifications/unread-count` — quick count for UI badge

---

## Testing

**Secrets**:
- Unit: encrypt/decrypt round-trip, secret template resolution (`${{ secrets.X }}` replacement)
- Integration:
  - Create secret → list (verify value not returned) → resolve internally (verify plaintext matches)
  - Versioning: update secret → verify version incremented
  - Scope: pipeline secret not resolvable with agent scope
  - Project scoping: project A secret not resolvable from project B

**Notifications**:
- Unit: HMAC computation for webhooks
- Integration:
  - In-app notification: create → list → mark read → verify
  - Email: send to test SMTP (can use in-memory smtp mock or skip in CI)
  - Webhook: deliver to test endpoint → verify payload + HMAC signature
  - Event-driven: trigger build failure → verify notification created for project owner

## Done When

1. Secrets encrypted at rest with AES-256-GCM
2. Secret CRUD API (write-only — values never returned via API)
3. Secret resolution works for pipeline/agent configs
4. In-app notifications stored and queryable
5. Email notifications sent via SMTP
6. Webhook delivery with HMAC signing
7. Event-driven notification helpers callable from other modules

## Security Context (from security hardening)

All new handlers must follow the security patterns established in the codebase:

### Secrets-specific security

- **Input validation**: Secret names must be validated (1-255, alphanumeric + `-_`). Secret values have a max size (e.g., 64KB). Scope must be one of the allowed values. See `CLAUDE.md` Security Patterns for field limits.
- **Master key management**: `PLATFORM_MASTER_KEY` must be validated at startup (exactly 32 bytes hex-encoded). Panic in production mode if missing or invalid. In dev mode, derive a deterministic key from a known value.
- **Encrypt at rest**: All secret values stored via AES-256-GCM. The nonce must be randomly generated per encryption (never reuse nonces). Use `aes-gcm` crate.
- **Write-only API**: Secret values must **never** be returned via API. List endpoints return metadata only (name, scope, version, created_at). The `resolve_secret()` function is internal-only.
- **Scope enforcement**: Pipeline secrets must not be resolvable by agent scope and vice versa. Enforce scope matching in `resolve_secrets_for_env()`.
- **Audit logging**: Log secret create/delete but **never** log secret values. Audit detail should contain only the secret name and scope.
- **Template injection**: The `${{ secrets.NAME }}` resolution in pipeline configs must only match the exact pattern — don't use general-purpose template engines that could allow code execution.

### Notifications-specific security

- **SSRF on webhook delivery**: Use the same SSRF protection pattern as `src/api/webhooks.rs` — block private IPs, metadata endpoints, non-HTTP schemes. Or reuse the shared `WEBHOOK_CLIENT` with timeouts.
- **Email injection**: When sending SMTP emails, sanitize the `to`, `subject`, and `body` fields. Don't allow header injection via newlines in subject/headers.
- **Rate limiting on notifications**: Prevent notification spam — limit the number of notifications per user per hour. A runaway alert rule or build loop could generate thousands of notifications.
- **Audit logging**: Log notification dispatch (type, channel, recipient) but never log email bodies or webhook payloads in audit detail.
- **Input validation**: Notification type, subject, body, and ref_type must all be length-validated. Channel must be one of the allowed values.

## Estimated LOC
~1,000 Rust (500 secrets + 500 notify)
