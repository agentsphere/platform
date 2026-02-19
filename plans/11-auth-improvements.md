# 11 — Auth Improvements: User Types, API Token Scope Enforcement & Passkeys

## Prerequisite
- 02-identity-auth complete (AuthUser, RBAC, delegation, API token CRUD)

## Blocks
- 07-agent-orchestration depends on user types (agent identity creation becomes cleaner)
- All modules benefit from scope-checked API tokens

## Can Parallelize With
- 03-09 parallel wave (this is an enhancement to the auth foundation, not a new module)

---

## Motivation

Phase 02 shipped a working auth system but left two gaps:

1. **All users are the same** — the `users` table has no concept of user type. Agent identities (created by 07-agent-orchestration) are just regular users with a naming convention (`agent-{session_id}`). Service accounts (CI runners, webhook consumers, cron jobs) are also just regular users. This makes it impossible to enforce type-specific policies: agents shouldn't be able to log in with a password, service accounts shouldn't spawn agents, humans need password rotation but agents don't have passwords at all.

2. **API token scopes are stored but never checked** — `api_tokens.scopes` is a `TEXT[]` column populated at creation time, but the auth middleware (`AuthUser` extractor) ignores scopes entirely. A token created with `scopes: ["project:read"]` can do writes if the underlying user has write permissions. This breaks the principle of least privilege and makes API tokens no safer than session cookies.

---

## Design Decisions

### User types: column vs. separate tables

**Decision: `user_type` column on `users` table** (not separate tables).

Rationale: All user types share the same identity, RBAC, audit, and token machinery. Separate tables would require JOIN-heavy auth lookups, duplicate FK relationships, and make the RBAC resolver aware of table polymorphism. A discriminator column keeps the data model simple and lets us enforce type-specific rules in application logic.

```
user_type TEXT NOT NULL DEFAULT 'human'
CHECK (user_type IN ('human', 'agent', 'service_account'))
```

### Agent vs. Service Account — what's the difference?

| Property | Human | Service Account | Agent |
|---|---|---|---|
| Has password | Yes | No | No |
| Can log in (session) | Yes | No | No |
| Can hold API tokens | Yes | Yes (primary auth) | Yes (ephemeral, auto-created) |
| Created by | Admin | Admin | System (07-agent-orchestration) |
| Lifetime | Permanent | Permanent | Ephemeral (tied to agent session) |
| Token scope enforcement | Yes | Yes | Yes |
| Can spawn agents | Yes | No | No (no recursive spawning) |
| Can be delegated to | Yes | Yes | Yes |
| Typical use | Interactive dev | CI pipeline, webhook, cron | Claude Code session |
| Permissions | Via roles | Via roles | Via delegation (scoped, time-limited) |
| Password rotation | Policy-driven | N/A | N/A |
| Max concurrent sessions | Unlimited | N/A | 1 per agent user |

Key insight: **service accounts are deterministic** — they run known scripts/pipelines with predictable behavior. **Agents are non-deterministic** — they make autonomous decisions, need guardrails (rate limits, cost caps, human-in-the-loop gates), and their permissions should be tightly scoped and time-limited by default. Capturing this distinction in the data model now means we can enforce different policies later without a migration.

### API token scope model

**Decision: Scopes mirror the Permission enum** — no separate scope taxonomy.

A token's `scopes` array contains permission strings (e.g., `["project:read", "project:write"]`). The special scope `"*"` means "all permissions the user has" (equivalent to no restriction — the token inherits the user's full permission set). An empty scopes array `[]` also means unrestricted (backward-compatible with existing tokens that were created with `DEFAULT '{}'`).

Scope enforcement happens in the `AuthUser` extractor: after identifying the user, attach the token's scopes to the `AuthUser` struct. The permission resolver then intersects the user's role-based permissions with the token's scopes.

```
effective_permissions = user_role_permissions ∩ token_scopes
```

If authenticated via session cookie (not API token), there is no scope restriction — sessions inherit full permissions.

---

## Deliverables

### 1. Migration: Add `user_type` column

```
migrations/20260220010022_user_type.up.sql
```

```sql
-- Add user_type discriminator
ALTER TABLE users
    ADD COLUMN user_type TEXT NOT NULL DEFAULT 'human';

-- Add CHECK constraint
ALTER TABLE users
    ADD CONSTRAINT chk_users_user_type
    CHECK (user_type IN ('human', 'agent', 'service_account'));

-- Add metadata column for type-specific config (JSON)
-- e.g., agent: { session_id, provider, cost_cap }
-- e.g., service_account: { description, owner_id }
ALTER TABLE users
    ADD COLUMN metadata JSONB;

-- Index for listing by type
CREATE INDEX idx_users_user_type ON users (user_type);

-- Backfill: existing users are all human (the DEFAULT handles this)
-- The bootstrap admin user will remain type 'human'
```

Down migration:
```sql
DROP INDEX idx_users_user_type;
ALTER TABLE users DROP CONSTRAINT chk_users_user_type;
ALTER TABLE users DROP COLUMN metadata;
ALTER TABLE users DROP COLUMN user_type;
```

### 2. `src/auth/user_type.rs` — User Type Enum & Policies

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum UserType {
    Human,
    Agent,
    ServiceAccount,
}

impl UserType {
    /// Whether this user type can authenticate via password/session
    pub fn can_login(self) -> bool {
        matches!(self, Self::Human)
    }

    /// Whether this user type can create agent sessions
    pub fn can_spawn_agents(self) -> bool {
        matches!(self, Self::Human)
    }

    /// Whether this user type requires a password hash
    pub fn requires_password(self) -> bool {
        matches!(self, Self::Human)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::ServiceAccount => "service_account",
        }
    }
}
```

### 3. Update `AuthUser` extractor — carry token scopes + user type

Current `AuthUser`:
```rust
pub struct AuthUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub ip_addr: Option<String>,
}
```

Updated `AuthUser`:
```rust
pub struct AuthUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub user_type: UserType,
    pub ip_addr: Option<String>,
    /// Token scopes if authenticated via API token.
    /// None = session auth (no scope restriction).
    /// Some(vec![]) or Some(vec!["*"]) = unrestricted token.
    /// Some(vec!["project:read", ...]) = scoped token.
    pub token_scopes: Option<Vec<String>>,
}
```

Changes to `FromRequestParts<AppState>` implementation:

- **Bearer token path**: Query now joins `users` to also fetch `user_type`. Return `scopes` from the `api_tokens` row in `AuthUser.token_scopes`.
- **Session cookie path**: Query now joins `users` to also fetch `user_type`. Set `token_scopes = None` (sessions are unrestricted).
- **Login enforcement**: If `user_type` is not `human`, reject session-based auth with `ApiError::Unauthorized`. Non-human users must use API tokens.

Updated `AuthLookup`:
```rust
struct AuthLookup {
    user_id: Uuid,
    user_name: String,
    user_type: String,  // parse to UserType
    is_active: bool,
    scopes: Option<Vec<String>>,  // only for token auth path
}
```

### 4. Update `rbac/resolver.rs` — scope intersection

Add a new public function:

```rust
/// Resolve effective permissions, intersected with optional token scopes.
/// If `token_scopes` is None (session auth), returns full role-based permissions.
/// If `token_scopes` contains "*" or is empty, returns full role-based permissions.
/// Otherwise, returns the intersection of role-based permissions and token scopes.
pub async fn effective_permissions_scoped(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
    token_scopes: Option<&[String]>,
) -> anyhow::Result<HashSet<Permission>>
```

Logic:
1. Call existing `effective_permissions()` to get role-based perms (uses cache).
2. If `token_scopes` is `None`, return full set.
3. If `token_scopes` contains `"*"` or is empty, return full set.
4. Otherwise, parse each scope string to `Permission`, collect into a `HashSet`, and return the intersection.

Add a scoped variant of `has_permission`:

```rust
pub async fn has_permission_scoped(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
    perm: Permission,
    token_scopes: Option<&[String]>,
) -> anyhow::Result<bool>
```

**Do not break existing `has_permission()`** — it remains for use cases where scopes don't apply (internal checks, delegation validation, etc.).

### 5. Update `rbac/middleware.rs` — scope-aware `require_permission`

The existing `require_permission` middleware calls `resolver::has_permission()`. Update it to call `has_permission_scoped()` instead, passing `auth.token_scopes.as_deref()`.

This is the single choke point — all route-layer permission checks automatically become scope-aware.

### 6. Update `api/users.rs` — inline permission checks become scope-aware

Every handler that currently calls `resolver::has_permission()` inline needs updating:

```rust
// Before
let is_admin = resolver::has_permission(&state.pool, &state.valkey, auth.user_id, None, Permission::AdminUsers).await?;

// After
let is_admin = resolver::has_permission_scoped(
    &state.pool, &state.valkey, auth.user_id, None,
    Permission::AdminUsers, auth.token_scopes.as_deref(),
).await?;
```

Affected handlers:
- `create_user` — also enforce: only `Human` users can be created via this endpoint
- `list_users`
- `get_user`
- `update_user`
- `deactivate_user`
- `login` — reject non-human user types before password check

### 7. Update `api/admin.rs` — scope-aware permission checks

Same pattern as above. `require_admin()` helper updated to use `has_permission_scoped()`.

Additionally, add `require_admin()` to accept `&AuthUser` and pass its `token_scopes`:

```rust
async fn require_admin(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool, &state.valkey, auth.user_id, None,
        Permission::AdminUsers, auth.token_scopes.as_deref(),
    ).await.map_err(ApiError::Internal)?;
    if !allowed { return Err(ApiError::Forbidden); }
    Ok(())
}
```

### 8. Update `api/users.rs` — token creation validates scopes

When creating an API token (`POST /api/tokens`), validate that the requested scopes are a subset of the user's actual permissions:

```rust
// In create_api_token handler:
if !scopes.is_empty() && !scopes.contains(&"*".to_string()) {
    let user_perms = resolver::effective_permissions(
        &state.pool, &state.valkey, auth.user_id, body.project_id,
    ).await.map_err(ApiError::Internal)?;

    let user_perm_strings: HashSet<&str> = user_perms.iter().map(|p| p.as_str()).collect();

    for scope in &scopes {
        if scope != "*" && !user_perm_strings.contains(scope.as_str()) {
            return Err(ApiError::BadRequest(
                format!("scope '{}' exceeds your permissions", scope),
            ));
        }
    }
}
```

### 9. New API endpoints for service accounts

Add to `api/admin.rs` (or a new `api/service_accounts.rs` if cleaner):

- `POST /api/admin/service-accounts` — create service account (admin:users)
  - Request: `{ name, email, description, scopes?, project_id? }`
  - Creates user with `user_type = 'service_account'`, `password_hash = '!disabled'` (non-matchable)
  - Optionally auto-creates an API token with given scopes
  - Returns: `{ user: UserResponse, token?: CreateTokenResponse }`

- `GET /api/admin/service-accounts` — list service accounts (admin:users)
  - Filters `users WHERE user_type = 'service_account'`

- `DELETE /api/admin/service-accounts/{id}` — deactivate + revoke all tokens (admin:users)

### 10. Update `UserResponse` — expose user type

```rust
pub struct UserResponse {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub email: String,
    pub user_type: UserType,  // NEW
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

### 11. Update bootstrap — mark seeded admin as `human`

No code change needed — the `DEFAULT 'human'` handles this. But update the `SYSTEM_ROLES` to add descriptions reflecting user types:

- `agent` role description: "System role for agent-type users — permissions granted via delegation"
- `developer` role: "Default role for human developers"

### 12. Update `api/users.rs` `CreateUserRequest` — add optional user_type

```rust
pub struct CreateUserRequest {
    pub name: String,
    pub email: String,
    pub password: Option<String>,  // Required for human, forbidden for agent/service_account
    pub display_name: Option<String>,
    pub user_type: Option<UserType>,  // Default: human
}
```

Validation:
- If `user_type == Human` (or None): `password` required
- If `user_type == Agent` or `ServiceAccount`: `password` must be `None`, store `password_hash = '!disabled'`

### 13. Prepare for 07-agent-orchestration

Update `agent` role (Phase 07 context — document the contract here):

The `agent/identity.rs` module (deliverable 3 in plan-07) creates ephemeral users. This plan ensures that:
- `create_agent_identity()` sets `user_type = 'agent'` on the user row
- `metadata` JSONB stores `{ "session_id": "...", "provider": "claude-code", "cost_cap_usd": 5.0 }`
- Agent users cannot log in (enforced by `UserType::can_login()` check in `login` handler)
- Agent users cannot spawn agents (enforced by `UserType::can_spawn_agents()` check in session creation — plan-07)
- Agent users get ephemeral scoped API tokens (scope enforcement now actually works)

---

## Part B: Passkey (WebAuthn) Support

### Motivation

Passwords are the weakest link in authentication — phishing, credential stuffing, and reuse attacks are the top vectors for account compromise. Passkeys (WebAuthn/FIDO2) provide phishing-resistant, passwordless authentication using public-key cryptography backed by platform authenticators (Touch ID, Windows Hello, YubiKey, phone-as-authenticator).

Adding passkey support gives human users a second authentication method and positions the platform for a passwordless-first future. Passkeys complement the user type system: only `Human` users can register passkeys (agents and service accounts use API tokens).

### Design Decisions

#### Crate choice: `webauthn-rs`

**Decision: Use `webauthn-rs` (v0.5+)** — the de facto Rust WebAuthn library, well-maintained, handles CBOR/COSE parsing, attestation validation, and challenge lifecycle. Alternatives (`passkey-rs`, hand-rolling) are less mature or require significant effort.

Dependencies to add to `Cargo.toml`:
```toml
webauthn-rs = { version = "0.5", features = ["danger-allow-state-serialisation"] }
webauthn-rs-proto = "0.5"
```

The `danger-allow-state-serialisation` feature enables serializing registration/authentication state to store in Valkey between the two-step ceremony. Despite the "danger" prefix, this is the standard approach — the feature name is a reminder to store state server-side (which we do, in Valkey), never client-side.

#### Passkeys as second factor vs. primary auth

**Decision: Passkeys as a standalone login method** — not just 2FA.

Users can log in with either:
1. Username + password (existing flow)
2. Passkey (new flow — no password needed)

Rationale: The WebAuthn spec supports "usernameless" flows via discoverable credentials (resident keys). This gives the best UX — the user clicks "Sign in with passkey", the browser shows the credential picker, done. No typing required.

We also support passkeys alongside passwords — a user can have both. The `users.password_hash` column remains required for backward compatibility; users who only want passkeys still get a `'!disabled'` sentinel (same as service accounts).

#### Challenge storage: Valkey (not DB)

**Decision: Store WebAuthn challenge state in Valkey** with a short TTL.

The WebAuthn ceremony is two-step:
1. Server sends a challenge → client signs it with the authenticator
2. Client sends the signed response → server verifies against the stored challenge

The challenge state lives ~60 seconds. Putting it in Postgres would create write amplification for ephemeral data. Valkey is the right fit: `webauthn:reg:{user_id}` and `webauthn:auth:{challenge_id}` with 120s TTL.

#### Credential storage: dedicated table

**Decision: New `passkey_credentials` table** (not JSONB in `users.metadata`).

Each user can have multiple passkeys (e.g., laptop Touch ID + YubiKey + phone). A dedicated table with proper indexing supports credential lookup by `credential_id` during authentication (the browser sends the credential ID, server must find the matching row).

---

### Deliverables

#### 14. Migration: `passkey_credentials` table

```
migrations/20260220010023_passkey_credentials.up.sql
```

```sql
CREATE TABLE passkey_credentials (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- WebAuthn credential ID (base64url-encoded), sent by browser during auth
    credential_id   BYTEA NOT NULL UNIQUE,
    -- COSE public key (CBOR-encoded)
    public_key      BYTEA NOT NULL,
    -- Signature counter for clone detection
    sign_count      BIGINT NOT NULL DEFAULT 0,
    -- Whether this is a discoverable credential (resident key)
    discoverable    BOOLEAN NOT NULL DEFAULT true,
    -- Transports hint: ["usb", "nfc", "ble", "internal", "hybrid"]
    transports      TEXT[] NOT NULL DEFAULT '{}',
    -- User-provided name: "MacBook Touch ID", "YubiKey 5C"
    name            TEXT NOT NULL,
    -- Attestation data (optional, stored for enterprise audit)
    attestation     BYTEA,
    -- Backup eligibility and state (from WebAuthn Level 3)
    backup_eligible BOOLEAN NOT NULL DEFAULT false,
    backup_state    BOOLEAN NOT NULL DEFAULT false,
    last_used_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_passkey_credentials_user ON passkey_credentials(user_id);
-- credential_id already has UNIQUE constraint = implicit index
```

Down migration:
```sql
DROP TABLE passkey_credentials;
```

#### 15. `src/auth/passkey.rs` — WebAuthn configuration & ceremonies

```rust
use webauthn_rs::prelude::*;
use webauthn_rs::Webauthn;

/// Initialize WebAuthn relying party from config.
pub fn build_webauthn(config: &Config) -> Result<Webauthn, WebauthnError> {
    let rp_id = config.webauthn_rp_id.as_str();       // e.g., "platform.example.com"
    let rp_origin = config.webauthn_rp_origin.as_str(); // e.g., "https://platform.example.com"
    let builder = WebauthnBuilder::new(rp_id, &Url::parse(rp_origin)?)?
        .rp_name(&config.webauthn_rp_name);            // e.g., "Platform"
    builder.build()
}
```

**Registration ceremony** (two-step):

```rust
/// Step 1: Begin passkey registration — returns challenge JSON for the browser.
/// Stores `PasskeyRegistration` state in Valkey (120s TTL).
pub async fn begin_registration(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    user_name: &str,
    display_name: &str,
    existing_credentials: Vec<CredentialID>,  // to exclude already-registered keys
) -> Result<CreationChallengeResponse, AuthError>

/// Step 2: Complete passkey registration — verifies the browser's response,
/// returns the credential to store in DB.
pub async fn finish_registration(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    response: &RegisterPublicKeyCredential,
) -> Result<Passkey, AuthError>
```

**Authentication ceremony** (two-step):

```rust
/// Step 1: Begin passkey authentication — returns challenge JSON.
/// If `user_id` is None, uses discoverable credential flow (usernameless).
/// Stores `PasskeyAuthentication` state in Valkey (120s TTL).
pub async fn begin_authentication(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
    credentials: Option<Vec<Passkey>>,  // None = discoverable flow
) -> Result<(RequestChallengeResponse, PasskeyAuthentication), AuthError>

/// Step 2: Complete passkey authentication — verifies the signed challenge.
/// Returns the authenticated credential ID + updated counter.
pub async fn finish_authentication(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
    auth_state_key: &str,
    response: &PublicKeyCredential,
) -> Result<AuthenticationResult, AuthError>
```

**Valkey keys**:
- Registration state: `webauthn:reg:{user_id}` → serialized `PasskeyRegistration` (120s TTL)
- Authentication state: `webauthn:auth:{challenge_id}` → serialized `PasskeyAuthentication` (120s TTL)

#### 16. Config additions

Add to `Config` struct:

```rust
/// WebAuthn Relying Party ID (domain, no protocol). e.g., "platform.example.com"
#[clap(long, env = "WEBAUTHN_RP_ID", default_value = "localhost")]
pub webauthn_rp_id: String,

/// WebAuthn Relying Party Origin (full URL). e.g., "https://platform.example.com"
#[clap(long, env = "WEBAUTHN_RP_ORIGIN", default_value = "http://localhost:8080")]
pub webauthn_rp_origin: String,

/// WebAuthn Relying Party display name
#[clap(long, env = "WEBAUTHN_RP_NAME", default_value = "Platform")]
pub webauthn_rp_name: String,
```

Add `Webauthn` instance to `AppState`:

```rust
pub struct AppState {
    pub pool: PgPool,
    pub valkey: fred::clients::Pool,
    pub minio: opendal::Operator,
    pub kube: kube::Client,
    pub config: Arc<Config>,
    pub webauthn: Arc<Webauthn>,  // NEW
}
```

#### 17. API endpoints — `src/api/passkeys.rs`

All endpoints require `AuthUser` (must be logged in to manage passkeys). Authentication ceremony endpoints are unauthenticated (that's the point — you're logging in).

**Credential management** (authenticated):

```
POST   /api/auth/passkeys/register/begin    — start registration ceremony
POST   /api/auth/passkeys/register/complete  — finish registration ceremony
GET    /api/auth/passkeys                    — list user's passkeys
PATCH  /api/auth/passkeys/{id}               — rename a passkey
DELETE /api/auth/passkeys/{id}               — delete a passkey
```

**Authentication** (unauthenticated):

```
POST   /api/auth/passkey/login/begin     — start authentication ceremony
POST   /api/auth/passkey/login/complete   — finish authentication, return session
```

**Handler signatures**:

```rust
/// Begin registration — returns PublicKeyCredentialCreationOptions for navigator.credentials.create()
async fn begin_register(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<BeginRegisterRequest>,  // { name: "MacBook Touch ID" }
) -> Result<Json<CreationChallengeResponse>, ApiError>

/// Complete registration — browser sends attestation response
async fn complete_register(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<RegisterPublicKeyCredential>,
) -> Result<Json<PasskeyResponse>, ApiError>

/// Begin login — returns PublicKeyCredentialRequestOptions for navigator.credentials.get()
async fn begin_login(
    State(state): State<AppState>,
    // No auth — this IS the login flow
) -> Result<Json<BeginLoginResponse>, ApiError>

/// Complete login — verify assertion, create session, return token
async fn complete_login(
    State(state): State<AppState>,
    Json(body): Json<PublicKeyCredential>,
) -> Result<(StatusCode, HeaderMap, Json<LoginResponse>), ApiError>
```

**`begin_login` flow (discoverable credentials)**:
1. Call `webauthn.start_discoverable_authentication()` — no user lookup needed, the browser picks the credential.
2. Store `DiscoverableAuthentication` state in Valkey.
3. Return `RequestChallengeResponse` to browser.

**`complete_login` flow**:
1. Deserialize the `PublicKeyCredential` from the browser.
2. Look up `passkey_credentials` by `credential_id` from the response.
3. Load the Valkey auth state.
4. Call `webauthn.finish_discoverable_authentication()`.
5. Verify `sign_count` (clone detection): if the response counter ≤ stored counter, reject.
6. Update `sign_count` and `last_used_at` in DB.
7. Look up the `users` row (verify `is_active`, get `user_type`).
8. Create `auth_sessions` row + set cookie (same as password login).
9. Write audit log entry (`"auth.passkey_login"`).
10. Return `LoginResponse`.

**Request/Response types**:

```rust
#[derive(Debug, serde::Deserialize)]
pub struct BeginRegisterRequest {
    pub name: String,  // User-friendly name for the credential
}

#[derive(Debug, serde::Serialize)]
pub struct PasskeyResponse {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub backup_eligible: bool,
    pub backup_state: bool,
    pub transports: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct BeginLoginResponse {
    pub challenge: RequestChallengeResponse,
    pub challenge_id: String,  // Valkey key for the client to send back
}
```

#### 18. Passkey-only accounts (optional password)

Update the user model to support passwordless accounts:

- `users.password_hash` keeps `NOT NULL` constraint but accepts `'!disabled'` sentinel
- During user creation: if `user_type == Human` and passkeys are the intended auth method, allow `password = None` → store `'!disabled'`
- `login` handler: if `password_hash == '!disabled'`, return a clear error: `"This account uses passkey authentication. Use the passkey login flow."`
- Future: admin setting to require passkey for all users (org-level policy) — out of scope for this phase

#### 19. Update `src/api/mod.rs` — wire passkey routes

```rust
// In api/mod.rs router construction:
.merge(passkeys::router())
```

#### 20. Update `src/auth/mod.rs` — re-export passkey module

```rust
pub mod passkey;
```

---

## Migration Safety

This is an **additive, backward-compatible** change:
- `user_type DEFAULT 'human'` — existing users unaffected
- `metadata` — nullable, no backfill needed
- `token_scopes` in `AuthUser` — `None` for sessions preserves current behavior
- `effective_permissions_scoped()` with `None` scopes = same as `effective_permissions()`
- Existing API tokens with `scopes = '{}'` = unrestricted (backward-compatible)
- Existing `has_permission()` function preserved, not modified
- `passkey_credentials` — new table, no effect on existing auth flows
- `Webauthn` in `AppState` — additive, initialized at startup with sensible defaults
- Password login remains fully functional — passkeys are an alternative, not a replacement
- Users with no passkeys registered: zero change in behavior

---

## Testing

### Unit tests
- `UserType` — `can_login`, `can_spawn_agents`, `requires_password` for all three variants
- `UserType` — `as_str` / `FromStr` round-trip, serde round-trip
- Scope intersection logic — all cases:
  - `None` (session) → full perms
  - `Some(["*"])` → full perms
  - `Some([])` → full perms (backward compat)
  - `Some(["project:read"])` → only project:read from user's perms
  - `Some(["project:read", "nonexistent:perm"])` → only project:read (unknown scopes ignored)
  - User has `[A, B, C]`, token scopes `[B, C, D]` → effective = `[B, C]`

### Integration tests (`#[sqlx::test]`)
- Create human user → login succeeds
- Create service_account user → login fails with 401
- Create agent user → login fails with 401
- Create API token with scopes `["project:read"]` → can read projects, cannot write
- Create API token with scopes `["*"]` → can do everything user can
- Create API token with scopes `["admin:users"]` but user lacks admin:users → 400 bad request
- Service account with scoped token → correct permission intersection
- Session auth (cookie) → no scope restriction, full permissions
- Create service account via admin API → returns user + optional token
- List service accounts → only returns service_account type
- Deactivate service account → tokens revoked, auth fails

### Passkey tests

#### Unit tests
- `build_webauthn()` — succeeds with valid config, errors with invalid origin
- Valkey key formatting — correct TTL, correct key patterns
- `PasskeyResponse` serialization — round-trip serde

#### Integration tests (`#[sqlx::test]`)
- **Note**: Full WebAuthn ceremony requires a browser/authenticator. Integration tests use `webauthn-rs`'s test helpers or mock the CBOR attestation/assertion flow.
- Register passkey for human user → credential stored in DB
- Register passkey for service_account → 400 (only humans can register passkeys)
- List passkeys → returns only current user's credentials
- Delete passkey → credential removed, can no longer authenticate
- Rename passkey → name updated
- Login with passkey → session created, cookie set, audit logged
- Login with passkey for deactivated user → 401
- Clone detection: sign_count regression → 401, credential flagged
- Passkey-only account (password_hash = '!disabled') → password login returns clear error
- Multiple passkeys per user → any one works for login
- Challenge expiry: complete ceremony after 120s → 400 (state expired)

---

## Done When

### Part A: User Types & Token Scopes
1. `users` table has `user_type` column with CHECK constraint
2. `AuthUser` carries `user_type` and `token_scopes`
3. Login rejects non-human users
4. API token scopes enforced in permission checks (both inline and middleware)
5. Token creation validates scopes are subset of user's permissions
6. Service account CRUD endpoints work
7. All existing tests still pass (backward compatibility)
8. New unit + integration tests pass

### Part B: Passkeys
9. `passkey_credentials` table exists with proper indexes
10. `Webauthn` instance initialized in `AppState` from config
11. Registration ceremony works (begin → browser interaction → complete → credential stored)
12. Authentication ceremony works (begin → browser interaction → complete → session created)
13. Discoverable credential (usernameless) flow supported
14. Sign count validated on login (clone detection)
15. Passkey CRUD (list, rename, delete) works
16. Only `Human` users can register passkeys
17. Passkey-only accounts work (`password_hash = '!disabled'`, password login returns clear error)
18. Audit log entries for `auth.passkey_register`, `auth.passkey_login`, `auth.passkey_delete`
19. Challenge state stored in Valkey with 120s TTL

## Security Context (from security hardening)

This plan builds directly on the security hardening already applied. Key integration points:

### Already implemented (inherit these patterns)

- **Timing-safe login**: The `login` handler uses `password::dummy_hash()` for missing users — maintain this when adding the user type check. Run the argon2 verify even for non-human users to avoid timing leaks.
- **Rate limiting**: Login is rate-limited at 10 attempts/5min per username. Apply the same rate limiting to passkey authentication (`begin_login`/`complete_login`). Use `crate::auth::rate_limit::check_rate()` with prefix `"passkey_login"`.
- **Input validation**: All new endpoints must validate inputs. Passkey `name` field: 1-255 chars. Token `scopes` array: validate each scope string against `Permission::from_str()`. Service account `name`, `email`, `description`: use existing validation helpers.
- **Secure cookies**: Session creation from passkey login must include the `Secure` flag when `config.secure_cookies` is true (same as password login — reuse `create_login_session()` helper).
- **Session/token revocation**: When deactivating a service account or agent user, follow the existing pattern: delete all sessions + tokens + invalidate permission cache.

### New security considerations

- **WebAuthn challenge storage**: Store in Valkey with 120s TTL. Don't store challenge state in cookies or local storage — server-side only.
- **Sign count validation**: Always check the sign count on passkey authentication. If `response_counter <= stored_counter`, reject the authentication and flag the credential as potentially cloned.
- **Credential enumeration**: The `begin_login` discoverable flow should not reveal whether a user exists. Return the same challenge structure regardless.
- **Scope escalation prevention**: When creating API tokens, validate that requested scopes are a subset of the user's actual permissions. A user should not be able to create a token with more permissions than they have.
- **Agent user constraints**: Agent users created by plan-07 must not be able to: log in via password, register passkeys, create other agent sessions, or escalate their own permissions.
- **Audit logging**: Log all auth events: `auth.passkey_register`, `auth.passkey_login`, `auth.passkey_delete`, `service_account.create`, `service_account.deactivate`. Never log credential data, public keys, or challenge bytes.

## Estimated LOC
~900 Rust (Part A: ~500, Part B: ~400). Low-moderate risk — Part A is mostly wiring, Part B introduces a new dependency (`webauthn-rs`) but follows well-documented ceremony patterns.

## Files Modified

### Part A (User Types & Token Scopes)
- `migrations/20260220010022_user_type.{up,down}.sql` — NEW
- `src/auth/mod.rs` — re-export `user_type`
- `src/auth/user_type.rs` — NEW
- `src/auth/middleware.rs` — `AuthUser` struct + extractor changes
- `src/rbac/resolver.rs` — `effective_permissions_scoped`, `has_permission_scoped`
- `src/rbac/middleware.rs` — use scoped check
- `src/api/users.rs` — scope-aware checks, login type gate, token validation, `CreateUserRequest` changes
- `src/api/admin.rs` — scope-aware `require_admin`, service account endpoints
- `src/store/bootstrap.rs` — no changes needed (DEFAULT handles backfill)
- `.sqlx/` — regenerate after migration

### Part B (Passkeys)
- `Cargo.toml` — add `webauthn-rs`, `webauthn-rs-proto`, `url`
- `migrations/20260220010023_passkey_credentials.{up,down}.sql` — NEW
- `src/auth/mod.rs` — re-export `passkey`
- `src/auth/passkey.rs` — NEW (WebAuthn init, registration & auth ceremonies)
- `src/auth/middleware.rs` — no additional changes (session creation reused from password flow)
- `src/api/passkeys.rs` — NEW (6 endpoints: register begin/complete, login begin/complete, list, rename, delete)
- `src/api/mod.rs` — merge passkey routes
- `src/config.rs` — add `webauthn_rp_id`, `webauthn_rp_origin`, `webauthn_rp_name`
- `src/store/mod.rs` — `AppState` gains `webauthn: Arc<Webauthn>`
- `.sqlx/` — regenerate after migration
