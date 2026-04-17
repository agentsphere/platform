// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use fred::interfaces::ClientLike;
use fred::interfaces::EventInterface;
use fred::interfaces::PubsubInterface;
use serde::Serialize;
use tokio_stream::StreamExt;

use crate::api::helpers::require_admin;
use crate::state::PlatformState;
use platform_operator::health::{HealthSnapshot, SubsystemCheck, SubsystemStatus};
use platform_types::ApiError;
use platform_types::AuthUser;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct HealthSummary {
    pub overall: SubsystemStatus,
    pub subsystems: Vec<SubsystemCheck>,
    pub uptime_seconds: u64,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
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
    State(state): State<PlatformState>,
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
    State(state): State<PlatformState>,
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
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    let channel = "health:stream";

    // Dedicated subscriber connection for this SSE stream.
    // clone_new() creates an unconnected client — init() establishes the connection.
    let subscriber = state.valkey.next().clone_new();
    subscriber
        .init()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
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
