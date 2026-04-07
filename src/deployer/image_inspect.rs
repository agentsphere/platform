//! OCI image entrypoint resolution.
//!
//! Resolves `Entrypoint` + `Cmd` from OCI image configs for proxy wrapping.
//! Supports internal platform registry (DB lookup) and public external
//! registries (Docker Hub, GHCR, etc.) via OCI Distribution API.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::Deserialize;

/// Resolved entrypoint and cmd from an OCI image config.
#[derive(Debug, Clone)]
pub struct ImageEntrypoint {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
}

impl ImageEntrypoint {
    /// Full command line: entrypoint + cmd concatenated.
    pub fn full_command(&self) -> Vec<String> {
        let mut cmd = self.entrypoint.clone();
        cmd.extend(self.cmd.clone());
        cmd
    }

    pub fn is_empty(&self) -> bool {
        self.entrypoint.is_empty() && self.cmd.is_empty()
    }
}

/// Parsed image reference.
#[derive(Debug, Clone)]
pub struct ImageRef {
    /// Registry host (e.g. `registry-1.docker.io`, `ghcr.io`).
    pub registry: String,
    /// Repository path (e.g. `library/postgres`, `org/app`).
    pub repository: String,
    /// Tag or digest (e.g. `16`, `latest`, `sha256:abc...`).
    pub reference: String,
}

/// In-memory cache for resolved entrypoints.
pub struct EntrypointCache {
    entries: DashMap<String, CacheEntry>,
}

struct CacheEntry {
    entrypoint: ImageEntrypoint,
    inserted_at: Instant,
}

const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour

impl Default for EntrypointCache {
    fn default() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }
}

impl EntrypointCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, image_ref: &str) -> Option<ImageEntrypoint> {
        self.entries.get(image_ref).and_then(|entry| {
            if entry.inserted_at.elapsed() < CACHE_TTL {
                Some(entry.entrypoint.clone())
            } else {
                None
            }
        })
    }

    pub fn insert(&self, image_ref: String, entrypoint: ImageEntrypoint) {
        self.entries.insert(
            image_ref,
            CacheEntry {
                entrypoint,
                inserted_at: Instant::now(),
            },
        );
    }
}

/// Resolve entrypoint + cmd for a container image.
///
/// Strategy:
/// 1. Check in-memory cache
/// 2. Internal registry → DB + `MinIO` lookup
/// 3. Public registry → OCI Distribution API
/// 4. Returns None if resolution fails
#[tracing::instrument(skip(pool, minio, cache), fields(%image_ref))]
pub async fn resolve_entrypoint(
    image_ref: &str,
    pool: &sqlx::PgPool,
    minio: &opendal::Operator,
    platform_registry_url: Option<&str>,
    cache: &EntrypointCache,
) -> Option<ImageEntrypoint> {
    // 1. Cache hit
    if let Some(cached) = cache.get(image_ref) {
        return Some(cached);
    }

    // 2. Parse the image ref
    let parsed = parse_image_ref(image_ref);

    // 3. Try internal registry
    let result = if is_internal_image(&parsed, platform_registry_url) {
        resolve_from_internal(&parsed, pool, minio).await
    } else {
        resolve_from_public(&parsed).await
    };

    // 4. Cache on success
    if let Some(ref ep) = result
        && !ep.is_empty()
    {
        cache.insert(image_ref.to_string(), ep.clone());
    }

    result
}

/// Parse an image reference into registry/repo/tag components.
pub fn parse_image_ref(image: &str) -> ImageRef {
    // Strip digest if present (image@sha256:...)
    let (image_no_digest, _digest) = image.split_once('@').unwrap_or((image, ""));

    // Split tag
    let (image_no_tag, tag) = if let Some((img, t)) = image_no_digest.rsplit_once(':') {
        // Make sure the colon is for a tag, not a port (e.g., localhost:5000/repo)
        if t.contains('/') {
            (image_no_digest, "latest")
        } else {
            (img, t)
        }
    } else {
        (image_no_digest, "latest")
    };

    // Split registry from repository
    let (registry, repository) = if let Some((first, rest)) = image_no_tag.split_once('/') {
        if first.contains('.') || first.contains(':') {
            // Has a domain or port → it's a registry
            (first.to_string(), rest.to_string())
        } else {
            // Docker Hub shorthand: org/repo → registry-1.docker.io
            ("registry-1.docker.io".to_string(), image_no_tag.to_string())
        }
    } else {
        // Bare image name like "postgres" → Docker Hub library/
        (
            "registry-1.docker.io".to_string(),
            format!("library/{image_no_tag}"),
        )
    };

    ImageRef {
        registry,
        repository,
        reference: tag.to_string(),
    }
}

fn is_internal_image(parsed: &ImageRef, platform_registry_url: Option<&str>) -> bool {
    let Some(url) = platform_registry_url else {
        return false;
    };
    // Strip protocol
    let host = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    parsed.registry == host
}

// ---------------------------------------------------------------------------
// Internal registry resolution (DB + MinIO)
// ---------------------------------------------------------------------------

async fn resolve_from_internal(
    parsed: &ImageRef,
    pool: &sqlx::PgPool,
    minio: &opendal::Operator,
) -> Option<ImageEntrypoint> {
    // Find repository
    let repo_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM registry_repositories WHERE name = $1")
            .bind(&parsed.repository)
            .fetch_optional(pool)
            .await
            .ok()?;

    let repo_id = repo_id?;

    // Find manifest by tag
    let manifest_digest: Option<String> = sqlx::query_scalar(
        "SELECT manifest_digest FROM registry_tags WHERE repository_id = $1 AND name = $2",
    )
    .bind(repo_id)
    .bind(&parsed.reference)
    .fetch_optional(pool)
    .await
    .ok()?;

    let manifest_digest = manifest_digest?;

    // Fetch manifest content
    let manifest_content: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT content FROM registry_manifests WHERE repository_id = $1 AND digest = $2",
    )
    .bind(repo_id)
    .bind(&manifest_digest)
    .fetch_optional(pool)
    .await
    .ok()?;

    let manifest_content = manifest_content?;

    // Parse manifest to get config descriptor
    let config_digest = extract_config_digest(&manifest_content)?;

    // Fetch config blob from MinIO
    let minio_path: Option<String> = sqlx::query_scalar(
        "SELECT minio_path FROM registry_blobs b \
         JOIN registry_blob_repository_links l ON l.blob_digest = b.digest \
         WHERE l.repository_id = $1 AND b.digest = $2",
    )
    .bind(repo_id)
    .bind(&config_digest)
    .fetch_optional(pool)
    .await
    .ok()?;

    let minio_path = minio_path?;
    let config_bytes = minio.read(&minio_path).await.ok()?.to_vec();

    parse_image_config(&config_bytes)
}

// ---------------------------------------------------------------------------
// Public registry resolution (OCI Distribution API)
// ---------------------------------------------------------------------------

async fn resolve_from_public(parsed: &ImageRef) -> Option<ImageEntrypoint> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;

    // Get auth token (Docker Hub requires this, others may not)
    let token = fetch_registry_token(&client, &parsed.registry, &parsed.repository).await;

    // Fetch manifest
    let manifest_url = format!(
        "https://{}/v2/{}/manifests/{}",
        parsed.registry, parsed.repository, parsed.reference
    );

    let mut req = client.get(&manifest_url).header(
        "Accept",
        "application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.index.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json",
    );
    if let Some(ref t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!(
            status = %resp.status(), registry = %parsed.registry,
            repo = %parsed.repository, ref_ = %parsed.reference,
            "manifest fetch failed"
        );
        return None;
    }

    let manifest_bytes = resp.bytes().await.ok()?;

    // Check if this is an index/manifest list — if so, pick the amd64 manifest
    let manifest_bytes = resolve_manifest_index(
        &client,
        &manifest_bytes,
        &parsed.registry,
        &parsed.repository,
        token.as_deref(),
    )
    .await
    .unwrap_or(manifest_bytes.to_vec());

    // Extract config digest
    let config_digest = extract_config_digest(&manifest_bytes)?;

    // Fetch config blob
    let config_url = format!(
        "https://{}/v2/{}/blobs/{}",
        parsed.registry, parsed.repository, config_digest
    );
    let mut req = client.get(&config_url);
    if let Some(ref t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let config_bytes = resp.bytes().await.ok()?;

    parse_image_config(&config_bytes)
}

/// Fetch a Bearer token for Docker Hub or registries that use token auth.
async fn fetch_registry_token(
    client: &reqwest::Client,
    registry: &str,
    repository: &str,
) -> Option<String> {
    #[derive(Deserialize)]
    struct TokenResponse {
        token: String,
    }

    // Docker Hub uses a separate auth service
    if registry != "registry-1.docker.io" {
        return None;
    }

    let auth_url = format!(
        "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repository}:pull"
    );
    let resp = client.get(&auth_url).send().await.ok()?;
    let token_resp: TokenResponse = resp.json().await.ok()?;
    Some(token_resp.token)
}

/// If the manifest is an OCI index or Docker manifest list, resolve the
/// amd64/linux manifest from it.
async fn resolve_manifest_index(
    client: &reqwest::Client,
    manifest_bytes: &[u8],
    registry: &str,
    repository: &str,
    token: Option<&str>,
) -> Option<Vec<u8>> {
    #[derive(Deserialize)]
    struct Index {
        manifests: Option<Vec<IndexEntry>>,
    }

    #[derive(Deserialize)]
    struct IndexEntry {
        digest: String,
        platform: Option<Platform>,
    }

    #[derive(Deserialize)]
    struct Platform {
        architecture: Option<String>,
        os: Option<String>,
    }

    let index: Index = serde_json::from_slice(manifest_bytes).ok()?;
    let manifests = index.manifests?;

    // Prefer amd64/linux
    let entry = manifests
        .iter()
        .find(|m| {
            m.platform.as_ref().is_some_and(|p| {
                p.architecture.as_deref() == Some("amd64") && p.os.as_deref() == Some("linux")
            })
        })
        .or_else(|| {
            // Fallback: arm64/linux
            manifests.iter().find(|m| {
                m.platform.as_ref().is_some_and(|p| {
                    p.architecture.as_deref() == Some("arm64") && p.os.as_deref() == Some("linux")
                })
            })
        })
        .or(manifests.first())?;

    // Fetch the platform-specific manifest
    let url = format!(
        "https://{registry}/v2/{repository}/manifests/{}",
        entry.digest
    );
    let mut req = client.get(&url).header(
        "Accept",
        "application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.v2+json",
    );
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    Some(resp.bytes().await.ok()?.to_vec())
}

// ---------------------------------------------------------------------------
// Shared parsing helpers
// ---------------------------------------------------------------------------

/// Extract the config descriptor digest from an OCI/Docker manifest.
fn extract_config_digest(manifest_bytes: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Manifest {
        config: Option<Descriptor>,
    }

    #[derive(Deserialize)]
    struct Descriptor {
        digest: String,
    }

    let manifest: Manifest = serde_json::from_slice(manifest_bytes).ok()?;
    manifest.config.map(|c| c.digest)
}

/// Parse OCI image config JSON for Entrypoint and Cmd.
fn parse_image_config(config_bytes: &[u8]) -> Option<ImageEntrypoint> {
    #[derive(Deserialize)]
    struct ImageConfig {
        config: Option<ContainerConfig>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ContainerConfig {
        entrypoint: Option<Vec<String>>,
        cmd: Option<Vec<String>>,
    }

    let config: ImageConfig = serde_json::from_slice(config_bytes).ok()?;
    let container_config = config.config?;

    Some(ImageEntrypoint {
        entrypoint: container_config.entrypoint.unwrap_or_default(),
        cmd: container_config.cmd.unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_image() {
        let r = parse_image_ref("postgres");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/postgres");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_image_with_tag() {
        let r = parse_image_ref("postgres:16");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/postgres");
        assert_eq!(r.reference, "16");
    }

    #[test]
    fn parse_image_with_org() {
        let r = parse_image_ref("bitnami/redis:7.2");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "bitnami/redis");
        assert_eq!(r.reference, "7.2");
    }

    #[test]
    fn parse_ghcr_image() {
        let r = parse_image_ref("ghcr.io/org/app:v1.0");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "org/app");
        assert_eq!(r.reference, "v1.0");
    }

    #[test]
    fn parse_localhost_registry() {
        let r = parse_image_ref("localhost:5000/my-app:dev");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "my-app");
        assert_eq!(r.reference, "dev");
    }

    #[test]
    fn parse_image_no_tag_defaults_to_latest() {
        let r = parse_image_ref("nginx");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_image_with_digest() {
        let r = parse_image_ref("nginx@sha256:abc123");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/nginx");
        // Digest is stripped, reference defaults to latest
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_ecr_image() {
        let r = parse_image_ref("123456789.dkr.ecr.us-east-1.amazonaws.com/my-app:v2");
        assert_eq!(r.registry, "123456789.dkr.ecr.us-east-1.amazonaws.com");
        assert_eq!(r.repository, "my-app");
        assert_eq!(r.reference, "v2");
    }

    #[test]
    fn is_internal_detects_platform_registry() {
        let parsed = parse_image_ref("localhost:8080/my-app:v1");
        assert!(is_internal_image(&parsed, Some("http://localhost:8080")));
        assert!(is_internal_image(&parsed, Some("localhost:8080")));
        assert!(!is_internal_image(&parsed, Some("localhost:9090")));
        assert!(!is_internal_image(&parsed, None));
    }

    #[test]
    fn parse_image_config_postgres() {
        let config_json = r#"{
            "config": {
                "Entrypoint": ["docker-entrypoint.sh"],
                "Cmd": ["postgres"]
            }
        }"#;
        let ep = parse_image_config(config_json.as_bytes()).unwrap();
        assert_eq!(ep.entrypoint, vec!["docker-entrypoint.sh"]);
        assert_eq!(ep.cmd, vec!["postgres"]);
        assert_eq!(ep.full_command(), vec!["docker-entrypoint.sh", "postgres"]);
    }

    #[test]
    fn parse_image_config_nginx() {
        let config_json = r#"{
            "config": {
                "Entrypoint": ["/docker-entrypoint.sh"],
                "Cmd": ["nginx", "-g", "daemon off;"]
            }
        }"#;
        let ep = parse_image_config(config_json.as_bytes()).unwrap();
        assert_eq!(
            ep.full_command(),
            vec!["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
    }

    #[test]
    fn parse_image_config_no_entrypoint() {
        let config_json = r#"{
            "config": {
                "Cmd": ["python", "app.py"]
            }
        }"#;
        let ep = parse_image_config(config_json.as_bytes()).unwrap();
        assert!(ep.entrypoint.is_empty());
        assert_eq!(ep.cmd, vec!["python", "app.py"]);
        assert_eq!(ep.full_command(), vec!["python", "app.py"]);
    }

    #[test]
    fn parse_image_config_empty() {
        let config_json = r#"{"config": {}}"#;
        let ep = parse_image_config(config_json.as_bytes()).unwrap();
        assert!(ep.is_empty());
    }

    #[test]
    fn extract_config_digest_oci() {
        let manifest = r#"{
            "schemaVersion": 2,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:abc123",
                "size": 1234
            },
            "layers": []
        }"#;
        assert_eq!(
            extract_config_digest(manifest.as_bytes()),
            Some("sha256:abc123".into())
        );
    }

    #[test]
    fn extract_config_digest_missing() {
        let manifest = r#"{"schemaVersion": 2}"#;
        assert_eq!(extract_config_digest(manifest.as_bytes()), None);
    }

    #[test]
    fn cache_insert_and_get() {
        let cache = EntrypointCache::new();
        let ep = ImageEntrypoint {
            entrypoint: vec!["sh".into()],
            cmd: vec!["-c".into()],
        };
        cache.insert("test:latest".into(), ep.clone());

        let cached = cache.get("test:latest").unwrap();
        assert_eq!(cached.entrypoint, vec!["sh"]);
        assert!(cache.get("missing:latest").is_none());
    }
}
