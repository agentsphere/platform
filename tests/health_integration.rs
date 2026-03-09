mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Health API Integration Tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn health_summary_requires_admin(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create a regular (non-admin) user
    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "regular", "regular@test.com").await;

    // Regular user should get forbidden
    let (status, _) = helpers::get_json(&app, &user_token, "/api/health").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Admin should succeed
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["overall"].is_string());
    assert!(body["subsystems"].is_array());
    assert!(body["uptime_seconds"].is_number());
}

#[sqlx::test(migrations = "./migrations")]
async fn health_summary_unauthenticated(pool: PgPool) {
    let (state, _) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(&app, "", "/api/health").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn health_details_returns_all_subsystem_names(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;

    // Populate the health snapshot by running a build_snapshot manually
    // The background loop may not have run yet, so write a snapshot directly
    {
        let mut snap = state.health.write().unwrap();
        *snap = platform::health::HealthSnapshot {
            overall: platform::health::SubsystemStatus::Degraded,
            subsystems: vec![
                platform::health::SubsystemCheck {
                    name: "postgres".into(),
                    status: platform::health::SubsystemStatus::Healthy,
                    latency_ms: 5,
                    message: None,
                    checked_at: chrono::Utc::now(),
                },
                platform::health::SubsystemCheck {
                    name: "valkey".into(),
                    status: platform::health::SubsystemStatus::Healthy,
                    latency_ms: 3,
                    message: None,
                    checked_at: chrono::Utc::now(),
                },
                platform::health::SubsystemCheck {
                    name: "minio".into(),
                    status: platform::health::SubsystemStatus::Healthy,
                    latency_ms: 10,
                    message: None,
                    checked_at: chrono::Utc::now(),
                },
                platform::health::SubsystemCheck {
                    name: "kubernetes".into(),
                    status: platform::health::SubsystemStatus::Healthy,
                    latency_ms: 20,
                    message: None,
                    checked_at: chrono::Utc::now(),
                },
                platform::health::SubsystemCheck {
                    name: "git_repos".into(),
                    status: platform::health::SubsystemStatus::Healthy,
                    latency_ms: 0,
                    message: None,
                    checked_at: chrono::Utc::now(),
                },
                platform::health::SubsystemCheck {
                    name: "secrets".into(),
                    status: platform::health::SubsystemStatus::Healthy,
                    latency_ms: 0,
                    message: None,
                    checked_at: chrono::Utc::now(),
                },
                platform::health::SubsystemCheck {
                    name: "registry".into(),
                    status: platform::health::SubsystemStatus::Degraded,
                    latency_ms: 0,
                    message: Some("registry not configured".into()),
                    checked_at: chrono::Utc::now(),
                },
            ],
            background_tasks: vec![],
            pod_failures: platform::health::PodFailureSummary {
                total_failed_24h: 0,
                agent_failures: 0,
                pipeline_failures: 0,
                recent_failures: vec![],
            },
            uptime_seconds: 42,
            checked_at: chrono::Utc::now(),
        };
    }

    let app = helpers::test_router(state);
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/health/details").await;
    assert_eq!(status, StatusCode::OK);

    // Verify all 7 subsystem names are present
    let subsystems = body["subsystems"].as_array().unwrap();
    let names: Vec<&str> = subsystems
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"postgres"));
    assert!(names.contains(&"valkey"));
    assert!(names.contains(&"minio"));
    assert!(names.contains(&"kubernetes"));
    assert!(names.contains(&"git_repos"));
    assert!(names.contains(&"secrets"));
    assert!(names.contains(&"registry"));
    assert_eq!(subsystems.len(), 7);

    // Verify overall status
    assert_eq!(body["overall"].as_str().unwrap(), "degraded");
    assert_eq!(body["uptime_seconds"].as_u64().unwrap(), 42);

    // Verify pod failures structure
    assert_eq!(body["pod_failures"]["total_failed_24h"], 0);
    assert_eq!(body["pod_failures"]["agent_failures"], 0);
    assert_eq!(body["pod_failures"]["pipeline_failures"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn health_details_requires_admin(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "viewer", "viewer@test.com").await;
    let (status, _) = helpers::get_json(&app, &user_token, "/api/health/details").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn readyz_returns_ok_without_auth(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;

    // Pre-populate a healthy snapshot so readyz uses cached data
    {
        let mut snap = state.health.write().unwrap();
        snap.checked_at = chrono::Utc::now();
        snap.subsystems = vec![
            platform::health::SubsystemCheck {
                name: "postgres".into(),
                status: platform::health::SubsystemStatus::Healthy,
                latency_ms: 5,
                message: None,
                checked_at: chrono::Utc::now(),
            },
            platform::health::SubsystemCheck {
                name: "valkey".into(),
                status: platform::health::SubsystemStatus::Healthy,
                latency_ms: 3,
                message: None,
                checked_at: chrono::Utc::now(),
            },
        ];
    }

    let app = helpers::test_router(state);

    // readyz should work without authentication
    let req = axum::http::Request::builder()
        .uri("/readyz")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn readyz_returns_503_when_unhealthy(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;

    // Pre-populate an unhealthy snapshot
    {
        let mut snap = state.health.write().unwrap();
        snap.checked_at = chrono::Utc::now();
        snap.subsystems = vec![
            platform::health::SubsystemCheck {
                name: "postgres".into(),
                status: platform::health::SubsystemStatus::Unhealthy,
                latency_ms: 0,
                message: Some("connection refused".into()),
                checked_at: chrono::Utc::now(),
            },
            platform::health::SubsystemCheck {
                name: "valkey".into(),
                status: platform::health::SubsystemStatus::Healthy,
                latency_ms: 3,
                message: None,
                checked_at: chrono::Utc::now(),
            },
        ];
    }

    let app = helpers::test_router(state);

    let req = axum::http::Request::builder()
        .uri("/readyz")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
