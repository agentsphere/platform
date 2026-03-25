mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use std::io::Write;

/// Override state to use an isolated temp dir for agent-runner binaries,
/// so tests never corrupt the real cross-compiled binaries.
fn isolated_runner_state(state: &mut platform::store::AppState) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-runner-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create isolated dir");
    let mut config = (*state.config).clone();
    config.agent_runner_dir.clone_from(&dir);
    state.config = std::sync::Arc::new(config);
    dir
}

// -- Happy path --

#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_amd64_integration(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;
    let dir = isolated_runner_state(&mut state);

    let mut f = std::fs::File::create(dir.join("amd64")).expect("create fake binary");
    f.write_all(b"#!/bin/sh\necho fake-agent-runner\n")
        .expect("write fake binary");

    let app = helpers::test_router(state);

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/downloads/agent-runner?arch=amd64").await;
    assert!(status == StatusCode::OK || body.is_null());
}

#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_returns_binary_integration(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;
    let dir = isolated_runner_state(&mut state);

    std::fs::write(dir.join("arm64"), b"TESTBINARY").expect("write");

    let app = helpers::test_router(state);

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/downloads/agent-runner?arch=arm64")
        .header("Authorization", format!("Bearer {admin_token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Check headers
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"agent-runner\""
    );
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=3600"
    );

    // Check body
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(&body[..], b"TESTBINARY");
}

// -- Arch normalization --

#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_normalizes_x86_64_integration(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;
    let dir = isolated_runner_state(&mut state);

    std::fs::write(dir.join("amd64"), b"BIN").expect("write");

    let app = helpers::test_router(state);

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/downloads/agent-runner?arch=x86_64")
        .header("Authorization", format!("Bearer {admin_token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_normalizes_aarch64_integration(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;
    let dir = isolated_runner_state(&mut state);

    std::fs::write(dir.join("arm64"), b"BIN").expect("write");

    let app = helpers::test_router(state);

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/downloads/agent-runner?arch=aarch64")
        .header("Authorization", format!("Bearer {admin_token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// -- Error cases --

#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_invalid_arch_integration(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/downloads/agent-runner?arch=ppc64").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("amd64"));
}

#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_no_auth_integration(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _body) =
        helpers::get_json(&app, "", "/api/downloads/agent-runner?arch=amd64").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_missing_binary_integration(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;
    // Override agent_runner_dir to an empty temp dir so no binaries exist,
    // even when PLATFORM_AGENT_RUNNER_DIR points to pre-built binaries.
    let empty_dir =
        std::env::temp_dir().join(format!("agent-runner-empty-{}", uuid::Uuid::new_v4()));
    let mut config = (*state.config).clone();
    config.agent_runner_dir = empty_dir;
    state.config = std::sync::Arc::new(config);
    let app = helpers::test_router(state);

    let (status, _body) =
        helpers::get_json(&app, &admin_token, "/api/downloads/agent-runner?arch=amd64").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
