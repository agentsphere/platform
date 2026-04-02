//! Integration tests for the mesh CA module.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// CA init creates root cert in DB
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mesh_ca_init_creates_root_cert(pool: PgPool) {
    let (state, _token) = helpers::test_state(pool.clone()).await;

    // Init mesh CA with the test state's config
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;
    config.mesh_ca_cert_ttl_secs = 3600;
    config.mesh_ca_root_ttl_days = 365;

    let ca = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("mesh CA init should succeed");

    // Verify root CA row exists in DB
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM mesh_ca")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, 1, "should have exactly one CA row");

    // Trust bundle should be a valid PEM
    let trust = ca.trust_bundle();
    assert!(
        trust.starts_with("-----BEGIN CERTIFICATE-----"),
        "trust bundle should be PEM"
    );
}

// ---------------------------------------------------------------------------
// CA init is idempotent (loads from DB on subsequent calls)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mesh_ca_init_idempotent(pool: PgPool) {
    let (state, _token) = helpers::test_state(pool.clone()).await;
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;

    // First init
    let ca1 = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("first init");

    // Second init — should load from DB, not create new
    let ca2 = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("second init");

    // Both should return the same trust bundle
    assert_eq!(ca1.trust_bundle(), ca2.trust_bundle());

    // Should still be only one CA row
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM mesh_ca")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, 1);
}

// ---------------------------------------------------------------------------
// Cert issuance returns valid PEM
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mesh_cert_issuance_returns_valid_pem(pool: PgPool) {
    let (state, _token) = helpers::test_state(pool.clone()).await;
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;

    let ca = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("CA init");

    let spiffe_id = platform::mesh::SpiffeId::new("default", "my-svc").unwrap();
    let bundle = ca
        .issue_cert(&pool, &spiffe_id, "default", "my-svc")
        .await
        .expect("cert issuance should succeed");

    assert!(
        bundle.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"),
        "cert should be PEM"
    );
    assert!(
        bundle.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"),
        "key should be PEM"
    );
    assert!(
        bundle.ca_pem.starts_with("-----BEGIN CERTIFICATE-----"),
        "CA cert should be PEM"
    );

    // Verify cert was recorded in mesh_certs table
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM mesh_certs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, 1, "should have one issued cert record");
}

// ---------------------------------------------------------------------------
// Trust bundle endpoint returns PEM
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn trust_bundle_endpoint_returns_pem(pool: PgPool) {
    let (mut state, token) = helpers::test_state(pool.clone()).await;

    // Init mesh CA and set on state
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;
    state.config = Arc::new(config.clone());

    let ca = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("CA init");
    state.mesh_ca = Some(Arc::new(ca));

    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &token, "/api/mesh/ca/trust-bundle").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["ca_pem"]
            .as_str()
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----"),
        "trust bundle response should contain PEM"
    );
}

// ---------------------------------------------------------------------------
// Issue endpoint requires auth
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn issue_endpoint_requires_auth(pool: PgPool) {
    let (mut state, _token) = helpers::test_state(pool.clone()).await;
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;
    state.config = Arc::new(config.clone());

    let ca = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("CA init");
    state.mesh_ca = Some(Arc::new(ca));

    let app = helpers::test_router(state);

    // Request with no auth
    let (status, _body) = helpers::post_json(
        &app,
        "",
        "/api/mesh/certs/issue",
        serde_json::json!({"namespace": "default", "service": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Issue endpoint validates input
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn issue_endpoint_validates_input(pool: PgPool) {
    let (mut state, token) = helpers::test_state(pool.clone()).await;
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;
    state.config = Arc::new(config.clone());

    let ca = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("CA init");
    state.mesh_ca = Some(Arc::new(ca));

    let app = helpers::test_router(state);

    // Empty namespace
    let (status, _body) = helpers::post_json(
        &app,
        &token,
        "/api/mesh/certs/issue",
        serde_json::json!({"namespace": "", "service": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Empty service
    let (status, _body) = helpers::post_json(
        &app,
        &token,
        "/api/mesh/certs/issue",
        serde_json::json!({"namespace": "default", "service": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Invalid characters
    let (status, _body) = helpers::post_json(
        &app,
        &token,
        "/api/mesh/certs/issue",
        serde_json::json!({"namespace": "ns/bad", "service": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Issue endpoint returns cert when mesh is enabled
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn issue_endpoint_returns_cert(pool: PgPool) {
    let (mut state, token) = helpers::test_state(pool.clone()).await;
    let mut config = (*state.config).clone();
    config.mesh_enabled = true;
    state.config = Arc::new(config.clone());

    let ca = platform::mesh::MeshCa::init(&pool, &config)
        .await
        .expect("CA init");
    state.mesh_ca = Some(Arc::new(ca));

    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/mesh/certs/issue",
        serde_json::json!({"namespace": "default", "service": "my-svc"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["cert_pem"].as_str().unwrap().contains("CERTIFICATE"));
    assert!(body["key_pem"].as_str().unwrap().contains("PRIVATE KEY"));
    assert!(body["ca_pem"].as_str().unwrap().contains("CERTIFICATE"));
    assert!(body["not_after"].as_str().is_some());
}

// ---------------------------------------------------------------------------
// Mesh endpoints return 503 when CA is not enabled
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn endpoints_return_503_when_disabled(pool: PgPool) {
    let (state, token) = helpers::test_state(pool).await;
    // mesh_ca is None by default in test_state
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(&app, &token, "/api/mesh/ca/trust-bundle").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/mesh/certs/issue",
        serde_json::json!({"namespace": "default", "service": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
