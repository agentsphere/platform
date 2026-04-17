// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use webauthn_rs::prelude::*;

use sqlx::Row;

use platform_auth::token;
use platform_types::validation;
use platform_types::{ApiError, AuditEntry, AuthUser, ListResponse, UserType, send_audit};

use crate::state::PlatformState;

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
    user_type: UserType,
    is_active: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    credential_id: Uuid,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
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
            delete(delete_passkey)
                .patch(rename_passkey)
                .get(get_passkey),
        )
        // Authentication (unauthenticated)
        .route("/api/auth/passkeys/login/begin", post(begin_login))
        .route("/api/auth/passkeys/login/complete", post(complete_login))
}

// ---------------------------------------------------------------------------
// Registration handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn begin_register(
    State(state): State<PlatformState>,
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

    let ccr = crate::passkey::begin_registration(
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
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<RegisterPublicKeyCredential>,
) -> Result<impl IntoResponse, ApiError> {
    let pk =
        crate::passkey::finish_registration(&state.webauthn, &state.valkey, auth.user_id, &body)
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "auth.passkey_register".into(),
            resource: "passkey_credential".into(),
            resource_id: Some(row.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

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
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<ListResponse<PasskeyResponse>>, ApiError> {
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

    let items: Vec<PasskeyResponse> = rows
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

    let total = i64::try_from(items.len()).unwrap_or(0);
    Ok(Json(ListResponse { items, total }))
}

async fn get_passkey(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<PasskeyResponse>, ApiError> {
    let row = sqlx::query(
        "SELECT id, name, created_at, last_used_at, backup_eligible, backup_state, transports
         FROM passkey_credentials WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(auth.user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("passkey".into()))?;

    Ok(Json(PasskeyResponse {
        id: row.get("id"),
        name: row.get("name"),
        created_at: row.get("created_at"),
        last_used_at: row.get("last_used_at"),
        backup_eligible: row.get("backup_eligible"),
        backup_state: row.get("backup_state"),
        transports: row.get("transports"),
    }))
}

#[tracing::instrument(skip(state), fields(%id, user_id = %auth.user_id), err)]
async fn rename_passkey(
    State(state): State<PlatformState>,
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "auth.passkey_rename".into(),
            resource: "passkey".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(serde_json::json!({"ok": true})))
}

#[tracing::instrument(skip(state), fields(%id, user_id = %auth.user_id), err)]
async fn delete_passkey(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "auth.passkey_delete".into(),
            resource: "passkey_credential".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Authentication handlers (unauthenticated)
// ---------------------------------------------------------------------------

async fn begin_login(
    State(state): State<PlatformState>,
) -> Result<Json<BeginLoginResponse>, ApiError> {
    // S39: Rate limit — each call creates a Valkey challenge object (120s TTL)
    platform_auth::rate_limit::check_rate(&state.valkey, "passkey_begin", "global", 60, 60).await?;

    let (rcr, challenge_id) =
        crate::passkey::begin_discoverable_authentication(&state.webauthn, &state.valkey)
            .await
            .map_err(ApiError::Internal)?;

    Ok(Json(BeginLoginResponse {
        challenge: rcr,
        challenge_id,
    }))
}

#[tracing::instrument(skip(state, body), err)]
async fn complete_login(
    State(state): State<PlatformState>,
    Json(body): Json<CompleteLoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // A62: No AuthUser in this unauthenticated flow; IP is not available without
    // ConnectInfo (requires into_make_service_with_connect_info on the router).
    // Passkey login audit entries use ip_addr: None for now.
    let ip_addr: Option<String> = None;

    // Rate-limit passkey login attempts (global key — challenge_id is per-ceremony
    // and would allow unlimited attempts across ceremonies)
    platform_auth::rate_limit::check_rate(&state.valkey, "passkey_login", "global", 50, 300)
        .await?;

    // Extract the credential ID from the request to pre-filter the DB query,
    // rather than loading ALL credentials for ALL active users.
    let cred_id_filter: Vec<u8> = body.credential.raw_id.to_vec();

    let cred_rows = sqlx::query(
        "SELECT pc.id, pc.user_id, pc.credential_id, pc.public_key, pc.sign_count, \
               u.is_active, u.name as user_name, u.user_type, \
               u.display_name, u.email, u.created_at as user_created_at, \
               u.updated_at as user_updated_at \
         FROM passkey_credentials pc \
         JOIN users u ON u.id = pc.user_id \
         WHERE pc.credential_id = $1 AND u.is_active = true",
    )
    .bind(&cred_id_filter)
    .fetch_all(&state.pool)
    .await?;

    // Build discoverable keys from stored credentials
    use sqlx::Row;
    let discoverable_keys: Vec<DiscoverableKey> = cred_rows
        .iter()
        .filter_map(|row| {
            let pk_bytes: Vec<u8> = row.get("public_key");
            let pk: Passkey = serde_json::from_slice(&pk_bytes).ok()?;
            Some(DiscoverableKey::from(pk))
        })
        .collect();

    if discoverable_keys.is_empty() {
        return Err(ApiError::Unauthorized);
    }

    let auth_result = crate::passkey::finish_discoverable_authentication(
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
        .find(|r| {
            let cid: Vec<u8> = r.get("credential_id");
            cid == cred_id_bytes
        })
        .ok_or(ApiError::Unauthorized)?;

    let row_id: Uuid = matched_row.get("id");
    let row_user_id: Uuid = matched_row.get("user_id");
    let row_sign_count: i64 = matched_row.get("sign_count");
    let row_user_name: String = matched_row.get("user_name");
    let row_user_type: String = matched_row.get("user_type");
    let row_display_name: Option<String> = matched_row.get("display_name");
    let row_email: String = matched_row.get("email");
    let row_is_active: bool = matched_row.get("is_active");
    let row_created_at: chrono::DateTime<chrono::Utc> = matched_row.get("user_created_at");
    let row_updated_at: chrono::DateTime<chrono::Utc> = matched_row.get("user_updated_at");

    // Clone detection: verify sign count
    let new_counter = auth_result.counter();
    if new_counter > 0 && row_sign_count > 0 && i64::from(new_counter) <= row_sign_count {
        tracing::warn!(
            credential_id = %row_id,
            stored = row_sign_count,
            received = new_counter,
            "passkey clone detection: counter regression"
        );
        return Err(ApiError::Unauthorized);
    }

    // Update sign count and last_used_at
    sqlx::query(
        "UPDATE passkey_credentials SET sign_count = $1, last_used_at = now() WHERE id = $2",
    )
    .bind(i64::from(new_counter))
    .bind(row_id)
    .execute(&state.pool)
    .await?;

    // Verify user type allows login
    let user_type: UserType = row_user_type
        .parse()
        .map_err(|e: anyhow::Error| ApiError::Internal(e))?;
    if !user_type.can_login() {
        return Err(ApiError::Unauthorized);
    }

    let login_user = PasskeyLoginUser {
        user_id: row_user_id,
        user_name: row_user_name,
        display_name: row_display_name,
        email: row_email,
        user_type,
        is_active: row_is_active,
        created_at: row_created_at,
        updated_at: row_updated_at,
        credential_id: row_id,
    };

    build_passkey_session(&state, &login_user, ip_addr).await
}

async fn build_passkey_session(
    state: &PlatformState,
    u: &PasskeyLoginUser,
    ip_addr: Option<String>,
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: u.user_id,
            actor_name: u.user_name.clone(),
            action: "auth.passkey_login".into(),
            resource: "session".into(),
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({"credential_id": u.credential_id})),
            ip_addr,
        },
    );

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

    let secure_flag = if state.config.auth.secure_cookies {
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
