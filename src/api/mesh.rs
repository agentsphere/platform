//! Mesh CA API endpoints for certificate issuance and trust bundle retrieval.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::helpers::require_admin;
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::mesh::error::MeshError;
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct IssueCertRequest {
    pub namespace: String,
    pub service: String,
}

#[derive(Debug, Serialize)]
pub struct IssueCertResponse {
    pub cert_pem: String,
    pub key_pem: String,
    pub ca_pem: String,
    pub not_after: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct TrustBundleResponse {
    pub ca_pem: String,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mesh/certs/issue", post(issue_cert))
        .route("/api/mesh/certs/renew", post(renew_cert))
        .route("/api/mesh/ca/trust-bundle", get(trust_bundle))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Issue a new leaf certificate for a service identity.
///
/// Requires admin permission. Rate limited to 100 requests per minute.
async fn issue_cert(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<IssueCertRequest>,
) -> Result<Json<IssueCertResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    // Rate limit: 100 per minute per user
    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "mesh_issue",
        &auth.user_id.to_string(),
        100,
        60,
    )
    .await?;

    issue_cert_inner(&state, &body).await
}

/// Renew a leaf certificate (stateless, same logic as issue).
///
/// Requires admin permission. Rate limited to 100 requests per minute.
async fn renew_cert(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<IssueCertRequest>,
) -> Result<Json<IssueCertResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "mesh_renew",
        &auth.user_id.to_string(),
        100,
        60,
    )
    .await?;

    issue_cert_inner(&state, &body).await
}

/// Shared issuance logic for issue and renew endpoints.
async fn issue_cert_inner(
    state: &AppState,
    body: &IssueCertRequest,
) -> Result<Json<IssueCertResponse>, ApiError> {
    // Validate input
    validation::check_name(&body.namespace)?;
    validation::check_name(&body.service)?;

    // Resolve mesh CA
    let mesh_ca = state.mesh_ca.as_ref().ok_or(MeshError::NotEnabled)?;

    let spiffe_id = crate::mesh::SpiffeId::new(&body.namespace, &body.service)?;
    let bundle = mesh_ca
        .issue_cert(&state.pool, &spiffe_id, &body.namespace, &body.service)
        .await?;

    Ok(Json(IssueCertResponse {
        cert_pem: bundle.cert_pem,
        key_pem: bundle.key_pem,
        ca_pem: bundle.ca_pem,
        not_after: bundle.not_after,
    }))
}

/// Return the root CA trust bundle (PEM).
///
/// Any authenticated user can retrieve the trust bundle.
async fn trust_bundle(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<TrustBundleResponse>, ApiError> {
    let mesh_ca = state.mesh_ca.as_ref().ok_or(MeshError::NotEnabled)?;

    Ok(Json(TrustBundleResponse {
        ca_pem: mesh_ca.trust_bundle().to_owned(),
    }))
}
