use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use fred::interfaces::EventInterface;
use fred::interfaces::PubsubInterface;
use serde::Serialize;
use tokio_stream::StreamExt;
use ts_rs::TS;

use crate::api::helpers::require_admin;
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::health::{HealthSnapshot, SubsystemCheck, SubsystemStatus};
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct HealthSummary {
    pub overall: SubsystemStatus,
    pub subsystems: Vec<SubsystemCheck>,
    #[ts(type = "number")]
    pub uptime_seconds: u64,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health_summary))
        .route("/api/health/details", get(health_details))
        .route("/api/health/stream", get(health_sse))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/health — admin-only summary (overall + subsystems).
#[tracing::instrument(skip(state), err)]
async fn health_summary(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<HealthSummary>, ApiError> {
    require_admin(&state, &auth).await?;

    let snap = state
        .health
        .read()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("health lock poisoned")))?
        .clone();

    Ok(Json(HealthSummary {
        overall: snap.overall,
        subsystems: snap.subsystems,
        uptime_seconds: snap.uptime_seconds,
    }))
}

/// GET /api/health/details — admin-only full snapshot.
#[tracing::instrument(skip(state), err)]
async fn health_details(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<HealthSnapshot>, ApiError> {
    require_admin(&state, &auth).await?;

    let snap = state
        .health
        .read()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("health lock poisoned")))?
        .clone();

    Ok(Json(snap))
}

/// GET /api/health/stream — admin-only SSE real-time health updates.
#[tracing::instrument(skip(state), err)]
async fn health_sse(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    let channel = "health:stream";

    // Dedicated subscriber connection for this SSE stream
    let subscriber = state.valkey.next().clone_new();
    subscriber
        .subscribe(channel)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let mut msg_rx = subscriber.message_rx();

    let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);

    let channel_owned = channel.to_owned();
    tokio::spawn(async move {
        while let Ok(msg) = msg_rx.recv().await {
            let text: String = match msg.value.convert() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if tx.send(text).await.is_err() {
                break;
            }
        }
        let _ = subscriber.unsubscribe(&channel_owned).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|text| Ok::<_, std::convert::Infallible>(Event::default().event("health").data(text)));

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
