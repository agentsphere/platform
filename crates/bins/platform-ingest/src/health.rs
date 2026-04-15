// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Rich health check for the ingest binary.
//!
//! Probes Postgres and Valkey connectivity, reads the alert-router degraded
//! flag, and returns structured JSON with an appropriate HTTP status code.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::state::IngestState;

/// Threshold above which a component is considered degraded (not unhealthy).
const DEGRADED_LATENCY_MS: u64 = 50;

/// Timeout for individual health probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentStatus {
    Ok,
    Degraded,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentHealth {
    pub status: ComponentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverallStatus {
    Ok,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: OverallStatus,
    pub postgres: ComponentHealth,
    pub valkey: ComponentHealth,
    pub alert_router: ComponentHealth,
}

// ---------------------------------------------------------------------------
// Status logic (pure, tested separately)
// ---------------------------------------------------------------------------

/// Derive the overall status from component statuses.
pub fn overall_status(
    pg: ComponentStatus,
    vk: ComponentStatus,
    alert: ComponentStatus,
) -> OverallStatus {
    if pg == ComponentStatus::Error || vk == ComponentStatus::Error {
        return OverallStatus::Unhealthy;
    }
    if pg == ComponentStatus::Degraded
        || vk == ComponentStatus::Degraded
        || alert == ComponentStatus::Degraded
        || alert == ComponentStatus::Error
    {
        return OverallStatus::Degraded;
    }
    OverallStatus::Ok
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn healthz_handler(State(state): State<IngestState>) -> impl IntoResponse {
    let (pg, vk) = tokio::join!(check_postgres(&state.pool), check_valkey(&state.valkey));

    let alert = if state.alert_router_degraded.load(Ordering::Relaxed) {
        ComponentHealth {
            status: ComponentStatus::Degraded,
            latency_ms: None,
            message: Some("alert router failed to load".into()),
        }
    } else {
        ComponentHealth {
            status: ComponentStatus::Ok,
            latency_ms: None,
            message: None,
        }
    };

    let status = overall_status(pg.status, vk.status, alert.status);
    let http_status = if status == OverallStatus::Unhealthy {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    let body = HealthResponse {
        status,
        postgres: pg,
        valkey: vk,
        alert_router: alert,
    };

    (http_status, Json(body))
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

async fn check_postgres(pool: &sqlx::PgPool) -> ComponentHealth {
    let start = Instant::now();
    let result = tokio::time::timeout(
        PROBE_TIMEOUT,
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool),
    )
    .await;
    let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
        Ok(Ok(_)) => {
            let status = if latency_ms < DEGRADED_LATENCY_MS {
                ComponentStatus::Ok
            } else {
                ComponentStatus::Degraded
            };
            ComponentHealth {
                status,
                latency_ms: Some(latency_ms),
                message: None,
            }
        }
        Ok(Err(e)) => ComponentHealth {
            status: ComponentStatus::Error,
            latency_ms: Some(latency_ms),
            message: Some(e.to_string()),
        },
        Err(_) => ComponentHealth {
            status: ComponentStatus::Error,
            latency_ms: Some(latency_ms),
            message: Some("timeout (>2s)".into()),
        },
    }
}

async fn check_valkey(valkey: &fred::clients::Pool) -> ComponentHealth {
    use fred::interfaces::ClientLike;

    let start = Instant::now();
    let result = tokio::time::timeout(PROBE_TIMEOUT, valkey.ping::<String>(None)).await;
    let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
        Ok(Ok(_)) => {
            let status = if latency_ms < DEGRADED_LATENCY_MS {
                ComponentStatus::Ok
            } else {
                ComponentStatus::Degraded
            };
            ComponentHealth {
                status,
                latency_ms: Some(latency_ms),
                message: None,
            }
        }
        Ok(Err(e)) => ComponentHealth {
            status: ComponentStatus::Error,
            latency_ms: Some(latency_ms),
            message: Some(e.to_string()),
        },
        Err(_) => ComponentHealth {
            status: ComponentStatus::Error,
            latency_ms: Some(latency_ms),
            message: Some("timeout (>2s)".into()),
        },
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_logic_all_ok() {
        assert_eq!(
            overall_status(
                ComponentStatus::Ok,
                ComponentStatus::Ok,
                ComponentStatus::Ok
            ),
            OverallStatus::Ok,
        );
    }

    #[test]
    fn status_logic_degraded_pg() {
        assert_eq!(
            overall_status(
                ComponentStatus::Degraded,
                ComponentStatus::Ok,
                ComponentStatus::Ok
            ),
            OverallStatus::Degraded,
        );
    }

    #[test]
    fn status_logic_degraded_valkey() {
        assert_eq!(
            overall_status(
                ComponentStatus::Ok,
                ComponentStatus::Degraded,
                ComponentStatus::Ok
            ),
            OverallStatus::Degraded,
        );
    }

    #[test]
    fn status_logic_degraded_alert_router() {
        assert_eq!(
            overall_status(
                ComponentStatus::Ok,
                ComponentStatus::Ok,
                ComponentStatus::Degraded
            ),
            OverallStatus::Degraded,
        );
    }

    #[test]
    fn status_logic_alert_error_is_degraded_not_unhealthy() {
        assert_eq!(
            overall_status(
                ComponentStatus::Ok,
                ComponentStatus::Ok,
                ComponentStatus::Error
            ),
            OverallStatus::Degraded,
        );
    }

    #[test]
    fn status_logic_unhealthy_pg() {
        assert_eq!(
            overall_status(
                ComponentStatus::Error,
                ComponentStatus::Ok,
                ComponentStatus::Ok
            ),
            OverallStatus::Unhealthy,
        );
    }

    #[test]
    fn status_logic_unhealthy_valkey() {
        assert_eq!(
            overall_status(
                ComponentStatus::Ok,
                ComponentStatus::Error,
                ComponentStatus::Ok
            ),
            OverallStatus::Unhealthy,
        );
    }

    #[test]
    fn status_logic_unhealthy_both() {
        assert_eq!(
            overall_status(
                ComponentStatus::Error,
                ComponentStatus::Error,
                ComponentStatus::Error
            ),
            OverallStatus::Unhealthy,
        );
    }

    #[test]
    fn health_response_serializes() {
        let resp = HealthResponse {
            status: OverallStatus::Ok,
            postgres: ComponentHealth {
                status: ComponentStatus::Ok,
                latency_ms: Some(2),
                message: None,
            },
            valkey: ComponentHealth {
                status: ComponentStatus::Ok,
                latency_ms: Some(1),
                message: None,
            },
            alert_router: ComponentHealth {
                status: ComponentStatus::Ok,
                latency_ms: None,
                message: None,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["postgres"]["status"], "ok");
        assert_eq!(json["postgres"]["latency_ms"], 2);
        assert_eq!(json["valkey"]["latency_ms"], 1);
        // latency_ms omitted for alert_router
        assert!(json["alert_router"]["latency_ms"].is_null());
    }

    #[test]
    fn health_response_degraded_serializes() {
        let resp = HealthResponse {
            status: OverallStatus::Degraded,
            postgres: ComponentHealth {
                status: ComponentStatus::Ok,
                latency_ms: Some(5),
                message: None,
            },
            valkey: ComponentHealth {
                status: ComponentStatus::Ok,
                latency_ms: Some(3),
                message: None,
            },
            alert_router: ComponentHealth {
                status: ComponentStatus::Degraded,
                latency_ms: None,
                message: Some("alert router failed to load".into()),
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "degraded");
        assert_eq!(json["alert_router"]["status"], "degraded");
        assert_eq!(
            json["alert_router"]["message"],
            "alert router failed to load"
        );
    }

    #[test]
    fn health_response_unhealthy_serializes() {
        let resp = HealthResponse {
            status: OverallStatus::Unhealthy,
            postgres: ComponentHealth {
                status: ComponentStatus::Error,
                latency_ms: Some(2001),
                message: Some("timeout (>2s)".into()),
            },
            valkey: ComponentHealth {
                status: ComponentStatus::Ok,
                latency_ms: Some(1),
                message: None,
            },
            alert_router: ComponentHealth {
                status: ComponentStatus::Ok,
                latency_ms: None,
                message: None,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "unhealthy");
        assert_eq!(json["postgres"]["status"], "error");
        assert_eq!(json["postgres"]["message"], "timeout (>2s)");
    }
}
