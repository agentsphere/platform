//! Integration tests for the OCI container registry (Phase 27).

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

use helpers::{assign_role, create_project, create_user, test_router, test_state};

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
    use sha2::Digest as _;
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, _admin_token) = test_state(pool).await;
    let app = test_router(state);

    // /v2/ is publicly accessible per OCI spec (version check)
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/v2/")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Push endpoints still require auth
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v2/some-project/blobs/uploads/")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Chunked blob upload: POST (start) → PATCH (chunk) → PUT (complete).
#[sqlx::test(migrations = "./migrations")]
async fn chunked_blob_upload(pool: PgPool) {
    use sha2::Digest as _;
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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

/// Bearer token auth works for the registry (verifies `lookup_api_token` path).
#[sqlx::test(migrations = "./migrations")]
async fn registry_auth_bearer_token(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let (_, api_token) =
        create_user_with_api_token(&app, &admin_token, "bearer-user", "bearer@test.com", &pool)
            .await;

    let (status, _, _) = registry_request(&app, &api_token, "GET", "/v2/").await;
    assert_eq!(status, StatusCode::OK);
}

/// Basic auth works for the registry (docker login sends user:password as base64).
#[sqlx::test(migrations = "./migrations")]
async fn registry_auth_basic_auth(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, _admin_token) = test_state(pool).await;
    let app = test_router(state);

    // Push endpoint rejects invalid tokens
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v2/some-project/blobs/uploads/")
        .header("Authorization", "Bearer plat_bogus_token_12345")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().contains_key("www-authenticate"));
}

/// Inactive user's token returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn registry_auth_inactive_user_401(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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

    // Push endpoint rejects inactive user's token
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v2/some-project/blobs/uploads/")
        .header("Authorization", format!("Bearer {api_token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().contains_key("www-authenticate"));
}

// ---------------------------------------------------------------------------
// Registry seeding tests
// ---------------------------------------------------------------------------

/// Build a minimal OCI layout tarball in memory for testing.
fn build_test_oci_tarball(config_json: &[u8], layer_data: &[u8]) -> (Vec<u8>, String) {
    use sha2::Digest as _;

    // Compute digests
    let config_digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(config_json)));
    let layer_digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(layer_data)));

    // Build manifest
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_json.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest,
            "size": layer_data.len()
        }]
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(&manifest_bytes))
    );

    // Build index.json
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest,
            "size": manifest_bytes.len()
        }]
    });
    let index_bytes = serde_json::to_vec(&index).unwrap();

    let oci_layout = br#"{"imageLayoutVersion":"1.0.0"}"#;

    // Build tar
    let mut builder = tar::Builder::new(Vec::new());

    let mut add_file = |path: &str, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    };

    add_file("oci-layout", oci_layout);
    add_file("index.json", &index_bytes);
    // Add blobs by digest
    add_file(
        &format!(
            "blobs/sha256/{}",
            config_digest.strip_prefix("sha256:").unwrap()
        ),
        config_json,
    );
    add_file(
        &format!(
            "blobs/sha256/{}",
            layer_digest.strip_prefix("sha256:").unwrap()
        ),
        layer_data,
    );
    add_file(
        &format!(
            "blobs/sha256/{}",
            manifest_digest.strip_prefix("sha256:").unwrap()
        ),
        &manifest_bytes,
    );

    builder.finish().unwrap();
    let tar_bytes = builder.into_inner().unwrap();

    (tar_bytes, manifest_digest)
}

#[sqlx::test(migrations = "./migrations")]
async fn seed_image_imports_blobs_and_manifest(pool: PgPool) {
    let (state, _token) = test_state(pool.clone()).await;

    // Look up system repo (auto-created by seed, project_id = NULL)
    let repo_id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM registry_repositories WHERE name = 'platform-runner'")
            .fetch_optional(&pool)
            .await
            .unwrap()
            .expect("platform-runner repo should exist after seed");

    // Build test tarball
    let config_json = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer_data = b"fake layer content for seed test";
    let (tar_bytes, manifest_digest) = build_test_oci_tarball(config_json, layer_data);

    // Write tarball to temp dir
    let dir = tempfile::tempdir().unwrap();
    let tar_path = dir.path().join("test.tar");
    std::fs::write(&tar_path, &tar_bytes).unwrap();

    // Seed the image (use unique tag — "latest" may already be seeded by test_state)
    let test_tag = "seed-test";
    let result =
        platform::registry::seed::seed_image(&pool, &state.minio, repo_id, &tar_path, test_tag)
            .await
            .unwrap();

    match result {
        platform::registry::seed::SeedResult::Imported {
            manifest_digest: digest,
            blob_count,
        } => {
            assert_eq!(digest, manifest_digest);
            assert_eq!(blob_count, 3); // config + layer + manifest
        }
        platform::registry::seed::SeedResult::AlreadyExists => {
            panic!("expected Imported, got AlreadyExists");
        }
    }

    // Verify tag exists in DB
    let tag_exists: bool = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM registry_tags WHERE repository_id = $1 AND name = $2)",
    )
    .bind(repo_id)
    .bind(test_tag)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(tag_exists, "tag should exist after seeding");

    // Verify manifest exists
    let manifest_exists: bool = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM registry_manifests WHERE repository_id = $1 AND digest = $2)",
    )
    .bind(repo_id)
    .bind(&manifest_digest)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(manifest_exists, "manifest should exist after seeding");

    // Verify our test blobs exist (at least 3: config + layer + manifest).
    // The real seed images may have added more blobs to this repo.
    let blob_link_count: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM registry_blob_links WHERE repository_id = $1",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        blob_link_count >= 3,
        "expected at least 3 blob links, got {blob_link_count}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn seed_image_is_idempotent(pool: PgPool) {
    let (state, _token) = test_state(pool.clone()).await;

    // System repo auto-created by seed (project_id = NULL)
    let repo_id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM registry_repositories WHERE name = 'platform-runner'")
            .fetch_optional(&pool)
            .await
            .unwrap()
            .expect("platform-runner repo should exist after seed");

    let config_json = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer_data = b"idempotent seed test layer";
    let (tar_bytes, _) = build_test_oci_tarball(config_json, layer_data);

    let dir = tempfile::tempdir().unwrap();
    let tar_path = dir.path().join("test.tar");
    std::fs::write(&tar_path, &tar_bytes).unwrap();

    // First seed
    let result1 = platform::registry::seed::seed_image(
        &pool,
        &state.minio,
        repo_id,
        &tar_path,
        "idempotent-tag",
    )
    .await
    .unwrap();
    assert!(matches!(
        result1,
        platform::registry::seed::SeedResult::Imported { .. }
    ));

    // Second seed — should return AlreadyExists
    let result2 = platform::registry::seed::seed_image(
        &pool,
        &state.minio,
        repo_id,
        &tar_path,
        "idempotent-tag",
    )
    .await
    .unwrap();
    assert!(matches!(
        result2,
        platform::registry::seed::SeedResult::AlreadyExists
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn seed_all_scans_directory(pool: PgPool) {
    let (state, _token) = test_state(pool.clone()).await;

    // Verify platform-runner repo exists (auto-created by seed)
    let repo_exists: bool = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM registry_repositories WHERE name = 'platform-runner')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        repo_exists,
        "platform-runner repo should exist from seed auto-create"
    );

    // Build a tarball named platform-runner.tar in a temp dir
    let config_json = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer_data = b"seed_all test layer content";
    let (tar_bytes, _) = build_test_oci_tarball(config_json, layer_data);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("platform-runner.tar"), &tar_bytes).unwrap();

    // Also add a file that should be skipped
    std::fs::write(dir.path().join("readme.txt"), b"not a tarball").unwrap();

    // Run seed_all
    platform::registry::seed::seed_all(&pool, &state.minio, dir.path())
        .await
        .unwrap();

    // Verify the tag was created
    let tag_exists: bool = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM registry_tags t
         JOIN registry_repositories r ON r.id = t.repository_id
         WHERE r.name = 'platform-runner' AND t.name = 'latest')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(tag_exists, "latest tag should exist after seed_all");
}

#[sqlx::test(migrations = "./migrations")]
async fn seed_all_skips_nonexistent_directory(pool: PgPool) {
    let (state, _token) = test_state(pool.clone()).await;

    // Should not error when directory doesn't exist
    let nonexistent = std::path::Path::new("/tmp/platform-seed-nonexistent-xyz");
    platform::registry::seed::seed_all(&pool, &state.minio, nonexistent)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Registry tag pattern scoping
// ---------------------------------------------------------------------------

/// Helper: create an API token with a `registry_tag_pattern` in the DB.
/// Returns the raw token string.
async fn create_token_with_tag_pattern(
    pool: &PgPool,
    user_id: uuid::Uuid,
    pattern: Option<&str>,
) -> String {
    let (raw_token, token_hash) = platform::auth::token::generate_api_token();
    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at, registry_tag_pattern)
         VALUES ($1, $2, $3, $4, now() + interval '1 hour', $5)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_id)
    .bind("tag-pattern-test")
    .bind(&token_hash)
    .bind(pattern)
    .execute(pool)
    .await
    .unwrap();
    raw_token
}

/// Helper: push a manifest, returning the status code (does NOT assert 201).
async fn registry_push_manifest_status(
    app: &axum::Router,
    token: &str,
    repo_name: &str,
    reference: &str,
    config_digest: &str,
    layer_digests: &[&str],
) -> StatusCode {
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
        .uri(format!("/v2/{repo_name}/manifests/{reference}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(axum::body::Body::from(body))
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    resp.status()
}

/// Token with matching tag pattern can push a manifest.
#[sqlx::test(migrations = "./migrations")]
async fn tag_pattern_allows_matching_push(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let _proj_id = create_project(&app, &admin_token, "myapp-dev", "private").await;

    // Get admin user ID
    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Create token with tag pattern that allows myapp-dev:session-*
    let scoped_token =
        create_token_with_tag_pattern(&pool, admin_id, Some("myapp-dev:session-*")).await;

    // Upload blobs first (with admin token — blob upload is not tag-scoped)
    let admin_api = {
        let (s, b) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-reg", "expires_in_days": 1 }),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        b["token"].as_str().unwrap().to_owned()
    };
    let config_digest = registry_upload_blob(&app, &admin_api, "myapp-dev", b"{}").await;
    let layer_digest = registry_upload_blob(&app, &admin_api, "myapp-dev", b"layer-data").await;

    // Push manifest with matching tag — should succeed
    let status = registry_push_manifest_status(
        &app,
        &scoped_token,
        "myapp-dev",
        "session-abc12345",
        &config_digest,
        &[&layer_digest],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "matching tag pattern should allow push"
    );
}

/// Token with tag pattern rejects push to non-matching tag.
#[sqlx::test(migrations = "./migrations")]
async fn tag_pattern_rejects_non_matching_push(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let _proj_id = create_project(&app, &admin_token, "myapp-dev2", "private").await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Token scoped to myapp-dev2:session-*
    let scoped_token =
        create_token_with_tag_pattern(&pool, admin_id, Some("myapp-dev2:session-*")).await;

    // Upload blobs with admin token
    let admin_api = {
        let (s, b) = helpers::post_json(
            &app,
            &admin_token,
            "/api/tokens",
            serde_json::json!({ "name": "admin-reg2", "expires_in_days": 1 }),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        b["token"].as_str().unwrap().to_owned()
    };
    let config_digest = registry_upload_blob(&app, &admin_api, "myapp-dev2", b"{}").await;
    let layer_digest = registry_upload_blob(&app, &admin_api, "myapp-dev2", b"layer2").await;

    // Push with non-matching tag — should be denied
    let status = registry_push_manifest_status(
        &app,
        &scoped_token,
        "myapp-dev2",
        "latest",
        &config_digest,
        &[&layer_digest],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-matching tag should be denied"
    );
}

/// Token without tag pattern (NULL) can push any tag (backward compat).
#[sqlx::test(migrations = "./migrations")]
async fn null_tag_pattern_allows_any_push(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let _proj_id = create_project(&app, &admin_token, "myapp-dev3", "private").await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Token with no tag pattern (NULL)
    let unscoped_token = create_token_with_tag_pattern(&pool, admin_id, None).await;

    // Upload blobs
    let config_digest = registry_upload_blob(&app, &unscoped_token, "myapp-dev3", b"{}").await;
    let layer_digest = registry_upload_blob(&app, &unscoped_token, "myapp-dev3", b"layer3").await;

    // Push with any tag — should succeed
    let status = registry_push_manifest_status(
        &app,
        &unscoped_token,
        "myapp-dev3",
        "latest",
        &config_digest,
        &[&layer_digest],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "NULL tag pattern should allow any push"
    );
}
