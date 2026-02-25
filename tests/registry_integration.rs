//! Integration tests for the OCI container registry (Phase 27).

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

use helpers::{admin_login, assign_role, create_project, create_user, test_router, test_state};

// ---------------------------------------------------------------------------
// Helpers for registry API calls
// ---------------------------------------------------------------------------

/// Upload a blob via monolithic POST. Returns the digest string.
async fn registry_upload_blob(
    app: &axum::Router,
    token: &str,
    project_name: &str,
    data: &[u8],
) -> String {
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(data);
    let digest = format!("sha256:{}", hex::encode(hash));

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/v2/{project_name}/blobs/uploads/?digest={digest}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .body(axum::body::Body::from(data.to_vec()))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "monolithic blob upload failed"
    );

    digest
}

/// Push a manifest referencing the given config + layer digests. Returns manifest digest.
async fn registry_push_manifest(
    app: &axum::Router,
    token: &str,
    project_name: &str,
    reference: &str,
    config_digest: &str,
    layer_digests: &[&str],
) -> String {
    let layers: Vec<serde_json::Value> = layer_digests
        .iter()
        .map(|d| {
            serde_json::json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": d,
                "size": 100,
            })
        })
        .collect();

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": 100,
        },
        "layers": layers,
    });

    let body = serde_json::to_vec(&manifest).unwrap();

    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("/v2/{project_name}/manifests/{reference}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(axum::body::Body::from(body.clone()))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    let status = resp.status();

    // Compute the digest of the manifest body for return
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(&body);
    let digest = format!("sha256:{}", hex::encode(hash));

    assert_eq!(status, StatusCode::CREATED, "manifest push failed");
    digest
}

/// Send a raw request with a given method to a registry path.
async fn registry_request(
    app: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let req = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

/// Create a user and return an API token (not a session token).
/// Registry auth uses API tokens, not session cookies.
async fn create_user_with_api_token(
    app: &axum::Router,
    admin_token: &str,
    name: &str,
    email: &str,
    pool: &PgPool,
) -> (uuid::Uuid, String) {
    let (user_id, session_token) = create_user(app, admin_token, name, email).await;

    // Create an API token for this user
    let (status, body) = helpers::post_json(
        app,
        &session_token,
        "/api/tokens",
        serde_json::json!({ "name": "registry-test", "expires_in_days": 30 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create api token failed: {body}"
    );
    let raw_token = body["token"].as_str().unwrap().to_owned();

    // Also assign developer role so they have registry:push + registry:pull
    assign_role(app, admin_token, user_id, "developer", None, pool).await;

    (user_id, raw_token)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// GET /v2/ returns 200 with docker-distribution-api-version header.
#[sqlx::test(migrations = "./migrations")]
async fn version_check_returns_200(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "reguser1", "reg1@test.com", &pool).await;

    let (status, headers, _) = registry_request(&app, &api_token, "GET", "/v2/").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("docker-distribution-api-version")
            .and_then(|v| v.to_str().ok()),
        Some("registry/2.0"),
    );
}

/// Monolithic blob upload: POST with ?digest → 201, then HEAD verifies.
#[sqlx::test(migrations = "./migrations")]
async fn monolithic_blob_upload(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let (_uid, _api_token) =
        create_user_with_api_token(&app, &admin_token, "reguser2", "reg2@test.com", &pool).await;

    // Create a project first (owner gets full registry access)
    let _proj_id = create_project(&app, &admin_token, "blob-test", "private").await;

    // Use admin's API token since admin is the project owner
    let admin_api_token = {
        let (status, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-reg-test", "expires_in_days": 30 }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "create admin api token: {body}"
        );
        body["token"].as_str().unwrap().to_owned()
    };

    let data = b"hello blob data";
    let digest = registry_upload_blob(&app, &admin_api_token, "blob-test", data).await;

    // HEAD blob should now return 200
    let (status, headers, _) = registry_request(
        &app,
        &admin_api_token,
        "HEAD",
        &format!("/v2/blob-test/blobs/{digest}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok()),
        Some(digest.as_str()),
    );
    assert_eq!(
        headers.get("content-length").and_then(|v| v.to_str().ok()),
        Some("15"), // len("hello blob data")
    );
}

/// HEAD/GET for nonexistent digest returns 404 OCI error.
#[sqlx::test(migrations = "./migrations")]
async fn blob_not_found_returns_404(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let admin_api_token = {
        let (status, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-reg-404", "expires_in_days": 30 }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        body["token"].as_str().unwrap().to_owned()
    };

    let _proj_id = create_project(&app, &admin_token, "blob404-test", "private").await;

    // Upload a blob so the repository exists
    registry_upload_blob(&app, &admin_api_token, "blob404-test", b"seed").await;

    let fake_digest = format!("sha256:{}", "ab".repeat(32));
    let (status, _, body) = registry_request(
        &app,
        &admin_api_token,
        "HEAD",
        &format!("/v2/blob404-test/blobs/{fake_digest}"),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "body: {}",
        String::from_utf8_lossy(&body)
    );
}

/// GET blob returns the actual data.
#[sqlx::test(migrations = "./migrations")]
async fn blob_get_returns_data(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let admin_api_token = {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-blob-get", "expires_in_days": 30 }),
        )
        .await;
        body["token"].as_str().unwrap().to_owned()
    };

    let _proj_id = create_project(&app, &admin_token, "blobget-test", "private").await;

    let data = b"get-me-back";
    let digest = registry_upload_blob(&app, &admin_api_token, "blobget-test", data).await;

    let (status, _, body) = registry_request(
        &app,
        &admin_api_token,
        "GET",
        &format!("/v2/blobget-test/blobs/{digest}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

/// Push a manifest referencing an existing blob, then pull it back.
#[sqlx::test(migrations = "./migrations")]
async fn manifest_push_and_pull(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let admin_api_token = {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-manifest", "expires_in_days": 30 }),
        )
        .await;
        body["token"].as_str().unwrap().to_owned()
    };

    let _proj_id = create_project(&app, &admin_token, "manifest-test", "private").await;

    // Upload config and layer blobs
    let config_digest =
        registry_upload_blob(&app, &admin_api_token, "manifest-test", b"config-data").await;
    let layer_digest =
        registry_upload_blob(&app, &admin_api_token, "manifest-test", b"layer-data").await;

    // Push manifest tagged as "latest"
    let manifest_digest = registry_push_manifest(
        &app,
        &admin_api_token,
        "manifest-test",
        "latest",
        &config_digest,
        &[&layer_digest],
    )
    .await;

    // Pull by tag
    let (status, headers, body) = registry_request(
        &app,
        &admin_api_token,
        "GET",
        "/v2/manifest-test/manifests/latest",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok()),
        Some(manifest_digest.as_str()),
    );

    // Body should be valid JSON manifest
    let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(manifest["schemaVersion"], 2);
}

/// PUT manifest referencing non-existent blob is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn manifest_push_missing_blob_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let admin_api_token = {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-misblob", "expires_in_days": 30 }),
        )
        .await;
        body["token"].as_str().unwrap().to_owned()
    };

    let _proj_id = create_project(&app, &admin_token, "misblob-test", "private").await;

    // Upload a real config blob but use a fake layer digest
    let config_digest =
        registry_upload_blob(&app, &admin_api_token, "misblob-test", b"config").await;
    let fake_layer = format!("sha256:{}", "ff".repeat(32));

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": 6,
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": fake_layer,
            "size": 100,
        }],
    });

    let req = axum::http::Request::builder()
        .method("PUT")
        .uri("/v2/misblob-test/manifests/bad")
        .header("Authorization", format!("Bearer {admin_api_token}"))
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&manifest).unwrap(),
        ))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// DELETE manifest → 202, then GET returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn manifest_delete(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let admin_api_token = {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-del-manifest", "expires_in_days": 30 }),
        )
        .await;
        body["token"].as_str().unwrap().to_owned()
    };

    let _proj_id = create_project(&app, &admin_token, "delman-test", "private").await;

    let config_digest = registry_upload_blob(&app, &admin_api_token, "delman-test", b"cfg").await;
    let layer_digest = registry_upload_blob(&app, &admin_api_token, "delman-test", b"lyr").await;

    let _manifest_digest = registry_push_manifest(
        &app,
        &admin_api_token,
        "delman-test",
        "v1",
        &config_digest,
        &[&layer_digest],
    )
    .await;

    // Delete by tag
    let (status, _, _) = registry_request(
        &app,
        &admin_api_token,
        "DELETE",
        "/v2/delman-test/manifests/v1",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // GET should now return 404
    let (status, _, _) = registry_request(
        &app,
        &admin_api_token,
        "GET",
        "/v2/delman-test/manifests/v1",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Tag list returns pushed tags with pagination.
#[sqlx::test(migrations = "./migrations")]
async fn tag_list(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let admin_api_token = {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-taglist", "expires_in_days": 30 }),
        )
        .await;
        body["token"].as_str().unwrap().to_owned()
    };

    let _proj_id = create_project(&app, &admin_token, "taglist-test", "private").await;

    let config_digest = registry_upload_blob(&app, &admin_api_token, "taglist-test", b"cfg").await;
    let layer_digest = registry_upload_blob(&app, &admin_api_token, "taglist-test", b"lyr").await;

    // Push with multiple tags
    for tag in ["v1", "v2", "latest"] {
        registry_push_manifest(
            &app,
            &admin_api_token,
            "taglist-test",
            tag,
            &config_digest,
            &[&layer_digest],
        )
        .await;
    }

    let (status, _, body) =
        registry_request(&app, &admin_api_token, "GET", "/v2/taglist-test/tags/list").await;

    assert_eq!(status, StatusCode::OK);
    let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp["name"], "taglist-test");
    let tags = resp["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 3);
}

/// Unauthenticated request to /v2/ returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn registry_requires_auth(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/v2/")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Chunked blob upload: POST (start) → PATCH (chunk) → PUT (complete).
#[sqlx::test(migrations = "./migrations")]
async fn chunked_blob_upload(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let admin_api_token = {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-chunked", "expires_in_days": 30 }),
        )
        .await;
        body["token"].as_str().unwrap().to_owned()
    };

    let _proj_id = create_project(&app, &admin_token, "chunked-test", "private").await;

    // Step 1: Start upload (POST without digest)
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v2/chunked-test/blobs/uploads/")
        .header("Authorization", format!("Bearer {admin_api_token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    // Step 2: PATCH chunk
    let chunk = b"chunked-content";
    let req = axum::http::Request::builder()
        .method("PATCH")
        .uri(&location)
        .header("Authorization", format!("Bearer {admin_api_token}"))
        .header("Content-Type", "application/octet-stream")
        .body(axum::body::Body::from(chunk.to_vec()))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Step 3: PUT complete with digest
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(chunk);
    let digest = format!("sha256:{}", hex::encode(hash));

    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("{location}?digest={digest}"))
        .header("Authorization", format!("Bearer {admin_api_token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify blob exists
    let (status, _, body) = registry_request(
        &app,
        &admin_api_token,
        "GET",
        &format!("/v2/chunked-test/blobs/{digest}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, chunk);
}

// ---------------------------------------------------------------------------
// Registry GC tests
// ---------------------------------------------------------------------------

/// GC removes orphaned blobs (no links, older than 24h).
#[sqlx::test(migrations = "./migrations")]
async fn gc_removes_orphaned_blobs(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_token = admin_login(&app).await;

    // Create project + API token
    create_project(&app, &admin_token, "gc-proj1", "private").await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "gcuser1", "gc1@test.com", &pool).await;

    // Upload a blob
    let data = b"orphan-blob-data";
    let digest = registry_upload_blob(&app, &api_token, "gc-proj1", data).await;

    // Remove all blob links (makes it orphaned)
    sqlx::query("DELETE FROM registry_blob_links WHERE blob_digest = $1")
        .bind(&digest)
        .execute(&pool)
        .await
        .unwrap();

    // Backdate blob to > 24h ago
    sqlx::query(
        "UPDATE registry_blobs SET created_at = now() - interval '25 hours' WHERE digest = $1",
    )
    .bind(&digest)
    .execute(&pool)
    .await
    .unwrap();

    // Run GC
    platform::registry::gc::collect_garbage(&state)
        .await
        .unwrap();

    // Verify blob is gone from DB
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM registry_blobs WHERE digest = $1")
        .bind(&digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0, "orphaned blob should be deleted");
}

/// GC skips blobs that still have links.
#[sqlx::test(migrations = "./migrations")]
async fn gc_skips_linked_blobs(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_token = admin_login(&app).await;

    create_project(&app, &admin_token, "gc-proj2", "private").await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "gcuser2", "gc2@test.com", &pool).await;

    let data = b"linked-blob-data";
    let digest = registry_upload_blob(&app, &api_token, "gc-proj2", data).await;

    // Backdate but keep the link
    sqlx::query(
        "UPDATE registry_blobs SET created_at = now() - interval '25 hours' WHERE digest = $1",
    )
    .bind(&digest)
    .execute(&pool)
    .await
    .unwrap();

    // Run GC
    platform::registry::gc::collect_garbage(&state)
        .await
        .unwrap();

    // Blob should still exist (has a link)
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM registry_blobs WHERE digest = $1")
        .bind(&digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1, "linked blob should NOT be deleted");
}

/// GC skips recent orphans (within 24h grace period).
#[sqlx::test(migrations = "./migrations")]
async fn gc_skips_recent_orphans(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_token = admin_login(&app).await;

    create_project(&app, &admin_token, "gc-proj3", "private").await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "gcuser3", "gc3@test.com", &pool).await;

    let data = b"recent-orphan-data";
    let digest = registry_upload_blob(&app, &api_token, "gc-proj3", data).await;

    // Remove links but DON'T backdate (within grace period)
    sqlx::query("DELETE FROM registry_blob_links WHERE blob_digest = $1")
        .bind(&digest)
        .execute(&pool)
        .await
        .unwrap();

    // Run GC
    platform::registry::gc::collect_garbage(&state)
        .await
        .unwrap();

    // Blob should still exist (within 24h grace period)
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM registry_blobs WHERE digest = $1")
        .bind(&digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count.0, 1,
        "recent orphan should NOT be deleted (grace period)"
    );
}

// ---------------------------------------------------------------------------
// Blob error paths
// ---------------------------------------------------------------------------

/// Monolithic upload with mismatched digest → error.
#[sqlx::test(migrations = "./migrations")]
async fn blob_digest_mismatch_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    create_project(&app, &admin_token, "mismatch-proj", "private").await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "mismatch-user", "mm@test.com", &pool).await;

    let data = b"some data";
    let wrong_digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!(
            "/v2/mismatch-proj/blobs/uploads/?digest={wrong_digest}"
        ))
        .header("Authorization", format!("Bearer {api_token}"))
        .header("Content-Type", "application/octet-stream")
        .body(axum::body::Body::from(data.to_vec()))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    // Should be 400 (DIGEST_INVALID)
    assert_ne!(
        resp.status(),
        StatusCode::CREATED,
        "mismatched digest should be rejected"
    );
}

/// HEAD /v2/{name}/manifests/{tag} returns correct headers.
#[sqlx::test(migrations = "./migrations")]
async fn head_manifest_returns_digest(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    create_project(&app, &admin_token, "head-proj", "private").await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "headuser", "head@test.com", &pool).await;

    // Upload config + layer blobs
    let config_data = b"config-data-head";
    let layer_data = b"layer-data-head";
    let config_digest = registry_upload_blob(&app, &api_token, "head-proj", config_data).await;
    let layer_digest = registry_upload_blob(&app, &api_token, "head-proj", layer_data).await;

    // Push manifest
    registry_push_manifest(
        &app,
        &api_token,
        "head-proj",
        "v1",
        &config_digest,
        &[&layer_digest],
    )
    .await;

    // HEAD manifest
    let (status, headers, body) =
        registry_request(&app, &api_token, "HEAD", "/v2/head-proj/manifests/v1").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.get("docker-content-digest").is_some(),
        "HEAD should include docker-content-digest"
    );
    assert!(headers.get("content-type").is_some());
    assert!(body.is_empty(), "HEAD should have no body");
}

/// PUT manifest with invalid JSON → error.
#[sqlx::test(migrations = "./migrations")]
async fn manifest_invalid_json_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    create_project(&app, &admin_token, "badjson-proj", "private").await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "badjson-user", "bj@test.com", &pool).await;

    let req = axum::http::Request::builder()
        .method("PUT")
        .uri("/v2/badjson-proj/manifests/bad")
        .header("Authorization", format!("Bearer {api_token}"))
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(axum::body::Body::from(b"not valid json".to_vec()))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::CREATED,
        "invalid JSON should be rejected"
    );
}

// ---------------------------------------------------------------------------
// Registry auth paths
// ---------------------------------------------------------------------------

/// Bearer token auth works for the registry (verifies lookup_api_token path).
#[sqlx::test(migrations = "./migrations")]
async fn registry_auth_bearer_token(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "bearer-user", "bearer@test.com", &pool)
            .await;

    let (status, _, _) = registry_request(&app, &api_token, "GET", "/v2/").await;
    assert_eq!(status, StatusCode::OK);
}

/// Basic auth works for the registry (docker login sends user:password as base64).
#[sqlx::test(migrations = "./migrations")]
async fn registry_auth_basic_auth(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "basic-user", "basic@test.com", &pool).await;

    // Encode as Basic auth: username:api_token
    let creds = format!("basic-user:{api_token}");
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &creds);

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/v2/")
        .header("Authorization", format!("Basic {encoded}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Invalid Bearer token returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn registry_auth_invalid_token_401(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);

    let (status, headers, _) =
        registry_request(&app, "plat_bogus_token_12345", "GET", "/v2/").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.contains_key("www-authenticate"));
}

/// Inactive user's token returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn registry_auth_inactive_user_401(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let (user_id, api_token) = create_user_with_api_token(
        &app,
        &admin_token,
        "inactive-user",
        "inactive@test.com",
        &pool,
    )
    .await;

    // Deactivate the user directly in DB
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("deactivate user");

    // Token should now be rejected
    let (status, headers, _) = registry_request(&app, &api_token, "GET", "/v2/").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.contains_key("www-authenticate"));
}
