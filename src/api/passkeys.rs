use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::auth::{passkey, token};
use crate::error::ApiError;
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BeginRegisterRequest {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct PasskeyResponse {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub backup_eligible: bool,
    pub backup_state: bool,
    pub transports: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BeginLoginResponse {
    pub challenge: RequestChallengeResponse,
    pub challenge_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CompleteLoginRequest {
    pub challenge_id: String,
    pub credential: PublicKeyCredential,
}

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub user: crate::api::users::UserResponse,
}

struct PasskeyLoginUser {
    user_id: Uuid,
    user_name: String,
    display_name: Option<String>,
    email: String,
    user_type: crate::auth::user_type::UserType,
    is_active: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    credential_id: Uuid,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        // Registration (authenticated)
        .route("/api/auth/passkeys/register/begin", post(begin_register))
        .route(
            "/api/auth/passkeys/register/complete",
            post(complete_register),
        )
        // Credential management (authenticated)
        .route("/api/auth/passkeys", get(list_passkeys))
        .route(
            "/api/auth/passkeys/{id}",
            delete(delete_passkey).patch(rename_passkey),
        )
        // Authentication (unauthenticated)
        .route("/api/auth/passkey/login/begin", post(begin_login))
        .route("/api/auth/passkey/login/complete", post(complete_login))
}

// ---------------------------------------------------------------------------
// Registration handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn begin_register(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<BeginRegisterRequest>,
) -> Result<Json<CreationChallengeResponse>, ApiError> {
    // Only human users can register passkeys
    if !auth.user_type.can_login() {
        return Err(ApiError::BadRequest(
            "only human users can register passkeys".into(),
        ));
    }

    validation::check_length("name", &body.name, 1, 255)?;

    // Get existing credential IDs to exclude
    let existing = sqlx::query_scalar!(
        r#"SELECT credential_id as "credential_id!" FROM passkey_credentials WHERE user_id = $1"#,
        auth.user_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let exclude_creds: Vec<CredentialID> = existing.into_iter().map(CredentialID::from).collect();

    // Get display name for registration
    let display_name = sqlx::query_scalar!(
        r#"SELECT COALESCE(display_name, name) as "dn!" FROM users WHERE id = $1"#,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let ccr = passkey::begin_registration(
        &state.webauthn,
        &state.valkey,
        auth.user_id,
        &auth.user_name,
        &display_name,
        exclude_creds,
    )
    .await
    .map_err(ApiError::Internal)?;

    Ok(Json(ccr))
}

#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn complete_register(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<RegisterPublicKeyCredential>,
) -> Result<impl IntoResponse, ApiError> {
    let pk = passkey::finish_registration(&state.webauthn, &state.valkey, auth.user_id, &body)
        .await
        .map_err(|e| ApiError::BadRequest(format!("registration failed: {e}")))?;

    // Store credential in DB
    let cred_id_bytes: Vec<u8> = pk.cred_id().to_vec();
    let public_key_bytes = serde_json::to_vec(&pk).map_err(|e| ApiError::Internal(e.into()))?;

    // Get the passkey name from the Valkey-stored registration context
    // (We don't have it here directly, so we'll use a default and let the user
    // rename via the PATCH endpoint. For now, query the begin_register name
    // from an earlier context). Since we can't carry the name through the
    // WebAuthn ceremony, use a sensible default.
    let name = format!("Passkey {}", &hex::encode(&cred_id_bytes[..4]));

    let row = sqlx::query!(
        r#"
        INSERT INTO passkey_credentials (user_id, credential_id, public_key, name)
        VALUES ($1, $2, $3, $4)
        RETURNING id, created_at
        "#,
        auth.user_id,
        cred_id_bytes,
        public_key_bytes,
        name,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "auth.passkey_register",
            resource: "passkey_credential",
            resource_id: Some(row.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(PasskeyResponse {
            id: row.id,
            name,
            created_at: row.created_at,
            last_used_at: None,
            backup_eligible: false,
            backup_state: false,
            transports: vec![],
        }),
    ))
}

// ---------------------------------------------------------------------------
// Credential management handlers
// ---------------------------------------------------------------------------

async fn list_passkeys(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<PasskeyResponse>>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT id, name, created_at, last_used_at, backup_eligible, backup_state, transports
        FROM passkey_credentials WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        auth.user_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| PasskeyResponse {
            id: r.id,
            name: r.name,
            created_at: r.created_at,
            last_used_at: r.last_used_at,
            backup_eligible: r.backup_eligible,
            backup_state: r.backup_state,
            transports: r.transports,
        })
        .collect();

    Ok(Json(items))
}

#[tracing::instrument(skip(state), fields(%id, user_id = %auth.user_id), err)]
async fn rename_passkey(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RenameRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validation::check_length("name", &body.name, 1, 255)?;

    let result = sqlx::query!(
        "UPDATE passkey_credentials SET name = $1 WHERE id = $2 AND user_id = $3",
        body.name,
        id,
        auth.user_id,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("passkey".into()));
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

#[tracing::instrument(skip(state), fields(%id, user_id = %auth.user_id), err)]
async fn delete_passkey(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result = sqlx::query!(
        "DELETE FROM passkey_credentials WHERE id = $1 AND user_id = $2",
        id,
        auth.user_id,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("passkey".into()));
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "auth.passkey_delete",
            resource: "passkey_credential",
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Authentication handlers (unauthenticated)
// ---------------------------------------------------------------------------

async fn begin_login(State(state): State<AppState>) -> Result<Json<BeginLoginResponse>, ApiError> {
    let (rcr, challenge_id) =
        passkey::begin_discoverable_authentication(&state.webauthn, &state.valkey)
            .await
            .map_err(ApiError::Internal)?;

    Ok(Json(BeginLoginResponse {
        challenge: rcr,
        challenge_id,
    }))
}

#[tracing::instrument(skip(state, body), err)]
async fn complete_login(
    State(state): State<AppState>,
    Json(body): Json<CompleteLoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Rate-limit passkey login attempts
    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "passkey_login",
        &body.challenge_id,
        10,
        300,
    )
    .await?;

    // Look up all passkey credentials to build the discoverable keys list
    let cred_rows = sqlx::query!(
        r#"
        SELECT pc.id, pc.user_id, pc.credential_id, pc.public_key, pc.sign_count,
               u.is_active, u.name as user_name, u.user_type,
               u.display_name, u.email, u.created_at as user_created_at,
               u.updated_at as user_updated_at
        FROM passkey_credentials pc
        JOIN users u ON u.id = pc.user_id
        WHERE u.is_active = true
        "#
    )
    .fetch_all(&state.pool)
    .await?;

    // Build discoverable keys from stored credentials
    let discoverable_keys: Vec<DiscoverableKey> = cred_rows
        .iter()
        .filter_map(|row| {
            let pk: Passkey = serde_json::from_slice(&row.public_key).ok()?;
            Some(DiscoverableKey::from(pk))
        })
        .collect();

    if discoverable_keys.is_empty() {
        return Err(ApiError::Unauthorized);
    }

    let (_auth_state, auth_result) = passkey::finish_discoverable_authentication(
        &state.webauthn,
        &state.valkey,
        &body.challenge_id,
        &body.credential,
        &discoverable_keys,
    )
    .await
    .map_err(|_| ApiError::Unauthorized)?;

    // Find the matching credential row by credential ID from the result
    let cred_id_bytes = auth_result.cred_id().to_vec();
    let matched_row = cred_rows
        .iter()
        .find(|r| r.credential_id == cred_id_bytes)
        .ok_or(ApiError::Unauthorized)?;

    // Clone detection: verify sign count
    let new_counter = auth_result.counter();
    let stored_counter = matched_row.sign_count;
    if new_counter > 0 && stored_counter > 0 && i64::from(new_counter) <= stored_counter {
        tracing::warn!(
            credential_id = %matched_row.id,
            stored = stored_counter,
            received = new_counter,
            "passkey clone detection: counter regression"
        );
        return Err(ApiError::Unauthorized);
    }

    // Update sign count and last_used_at
    sqlx::query!(
        "UPDATE passkey_credentials SET sign_count = $1, last_used_at = now() WHERE id = $2",
        i64::from(new_counter),
        matched_row.id,
    )
    .execute(&state.pool)
    .await?;

    // Verify user type allows login
    let user_type: crate::auth::user_type::UserType = matched_row
        .user_type
        .parse()
        .map_err(|e: anyhow::Error| ApiError::Internal(e))?;
    if !user_type.can_login() {
        return Err(ApiError::Unauthorized);
    }

    let login_user = PasskeyLoginUser {
        user_id: matched_row.user_id,
        user_name: matched_row.user_name.clone(),
        display_name: matched_row.display_name.clone(),
        email: matched_row.email.clone(),
        user_type,
        is_active: matched_row.is_active,
        created_at: matched_row.user_created_at,
        updated_at: matched_row.user_updated_at,
        credential_id: matched_row.id,
    };

    build_passkey_session(&state, &login_user).await
}

async fn build_passkey_session(
    state: &AppState,
    u: &PasskeyLoginUser,
) -> Result<
    (
        StatusCode,
        [(axum::http::header::HeaderName, String); 1],
        Json<LoginResponse>,
    ),
    ApiError,
> {
    let (raw_token, token_hash) = token::generate_session_token();
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(24);

    sqlx::query!(
        "INSERT INTO auth_sessions (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
        u.user_id,
        token_hash,
        expires_at,
    )
    .execute(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: u.user_id,
            actor_name: &u.user_name,
            action: "auth.passkey_login",
            resource: "session",
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({"credential_id": u.credential_id})),
            ip_addr: None,
        },
    )
    .await;

    let response = LoginResponse {
        token: raw_token.clone(),
        expires_at,
        user: crate::api::users::UserResponse {
            id: u.user_id,
            name: u.user_name.clone(),
            display_name: u.display_name.clone(),
            email: u.email.clone(),
            user_type: u.user_type,
            is_active: u.is_active,
            created_at: u.created_at,
            updated_at: u.updated_at,
        },
    };

    let secure_flag = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "session={raw_token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400{secure_flag}"
    );

    Ok((
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(response),
    ))
}
