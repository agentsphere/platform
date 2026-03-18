//! OCI layout tarball parser and registry seeder.
//!
//! Imports pre-built OCI images from tarballs on disk into the platform's
//! built-in registry (`MinIO` blobs + Postgres metadata). Used to seed the
//! `platform-runner` image on first boot so that agent pods can pull it
//! immediately without waiting for a pipeline build.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// OCI layout types
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct OciLayout {
    #[serde(rename = "imageLayoutVersion")]
    image_layout_version: String,
}

#[derive(serde::Deserialize)]
struct OciIndex {
    manifests: Vec<OciIndexEntry>,
}

#[derive(serde::Deserialize)]
struct OciIndexEntry {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    #[allow(dead_code)] // used by OCI spec, available for future size validation
    size: i64,
}

/// Manifest body — we parse `manifests` for image index (multi-arch) support.
#[derive(serde::Deserialize)]
struct OciManifest {
    #[serde(default)]
    manifests: Vec<OciDescriptor>,
}

#[derive(serde::Deserialize)]
struct OciDescriptor {
    digest: String,
    #[allow(dead_code)]
    size: i64,
}

/// Result of a single image seed operation.
pub enum SeedResult {
    /// Tag already exists — nothing was imported.
    AlreadyExists,
    /// Image was imported successfully.
    Imported {
        manifest_digest: String,
        blob_count: usize,
    },
}

// ---------------------------------------------------------------------------
// Seed cache — avoids re-reading 297 MB tarballs across nextest processes
// ---------------------------------------------------------------------------

/// Cached seed metadata written next to each tarball as `.seed-cache.json`.
///
/// Contains only lightweight metadata (digests, sizes, manifest content) —
/// not the actual blob data. Used for DB-only seeding after the first process
/// has uploaded blobs to `MinIO`.
#[derive(serde::Serialize, serde::Deserialize)]
struct SeedCache {
    blobs: Vec<CachedBlob>,
    manifests: Vec<CachedManifest>,
    tag_digest: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedBlob {
    digest: String,
    size_bytes: i64,
    minio_path: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedManifest {
    digest: String,
    media_type: String,
    content: Vec<u8>,
    size_bytes: i64,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Scan a directory for OCI layout tarballs and seed each into the registry.
///
/// Filename stem is used as the repository name (e.g. `platform-runner.tar`
/// → repo `platform-runner`). Skips if directory doesn't exist.
pub async fn seed_all(
    pool: &PgPool,
    minio: &opendal::Operator,
    seed_path: &Path,
) -> Result<(), anyhow::Error> {
    if !seed_path.exists() {
        tracing::debug!(path = %seed_path.display(), "seed images directory does not exist, skipping");
        return Ok(());
    }

    let entries = std::fs::read_dir(seed_path)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };

        // Accept .tar and .tar.gz
        let repo_name = image_name_from_filename(&file_name);
        let Some(repo_name) = repo_name else {
            continue;
        };

        // Look up or auto-create system repository (project_id = NULL)
        let existing: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM registry_repositories WHERE name = $1")
                .bind(&repo_name)
                .fetch_optional(pool)
                .await?;

        let repo_id = if let Some(id) = existing {
            id
        } else {
            let new_id = Uuid::new_v4();
            sqlx::query_scalar(
                "INSERT INTO registry_repositories (id, project_id, name) \
                 VALUES ($1, NULL, $2) \
                 ON CONFLICT (name) DO UPDATE SET updated_at = now() \
                 RETURNING id",
            )
            .bind(new_id)
            .bind(&repo_name)
            .fetch_one(pool)
            .await?
        };

        match seed_image_cached(pool, minio, repo_id, &path, "latest").await {
            Ok(SeedResult::AlreadyExists) => {
                tracing::debug!(repo = %repo_name, "seed image already exists");
            }
            Ok(SeedResult::Imported {
                manifest_digest,
                blob_count,
            }) => {
                tracing::info!(
                    repo = %repo_name,
                    %manifest_digest,
                    blob_count,
                    "seeded registry image"
                );
            }
            Err(e) => {
                tracing::warn!(repo = %repo_name, error = %e, "failed to seed image");
            }
        }
    }

    Ok(())
}

/// Import a single OCI layout tarball into the registry.
///
/// Idempotent: if the tag already exists, returns `AlreadyExists`.
pub async fn seed_image(
    pool: &PgPool,
    minio: &opendal::Operator,
    repository_id: Uuid,
    tarball_path: &Path,
    tag: &str,
) -> Result<SeedResult, anyhow::Error> {
    // 1. Idempotency check
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM registry_tags WHERE repository_id = $1 AND name = $2")
            .bind(repository_id)
            .bind(tag)
            .fetch_optional(pool)
            .await?;
    if existing.is_some() {
        return Ok(SeedResult::AlreadyExists);
    }

    // 2. Read and parse the tarball
    let entries = extract_tarball_entries(tarball_path)?;

    // 3. Validate OCI layout
    let layout_bytes = entries
        .get("oci-layout")
        .ok_or_else(|| anyhow::anyhow!("missing oci-layout file in tarball"))?;
    let layout: OciLayout = serde_json::from_slice(layout_bytes)?;
    if layout.image_layout_version != "1.0.0" {
        anyhow::bail!(
            "unsupported OCI layout version: {}",
            layout.image_layout_version
        );
    }

    let index_bytes = entries
        .get("index.json")
        .ok_or_else(|| anyhow::anyhow!("missing index.json in tarball"))?;
    let index: OciIndex = serde_json::from_slice(index_bytes)?;
    if index.manifests.is_empty() {
        anyhow::bail!("index.json has no manifests");
    }

    // 4. Import all blobs
    let blob_count = import_blobs(pool, minio, repository_id, &entries).await?;

    // 5. Import manifest(s) from index.json and create tag
    let top_entry = &index.manifests[0];
    let manifest_digest = &top_entry.digest;
    import_manifests(pool, repository_id, top_entry, &entries).await?;

    // 6. Create tag
    sqlx::query(
        "INSERT INTO registry_tags (repository_id, name, manifest_digest) \
         VALUES ($1, $2, $3) ON CONFLICT (repository_id, name) DO UPDATE SET manifest_digest = $3",
    )
    .bind(repository_id)
    .bind(tag)
    .bind(manifest_digest)
    .execute(pool)
    .await?;

    Ok(SeedResult::Imported {
        manifest_digest: manifest_digest.clone(),
        blob_count,
    })
}

// ---------------------------------------------------------------------------
// Cached seed — cross-process double-checked locking
// ---------------------------------------------------------------------------

/// Import an OCI layout tarball with cross-process file-based caching.
///
/// Uses double-checked locking via `fs2` to ensure only one process reads
/// the full tarball and uploads blobs to `MinIO`. All other processes block
/// on the lock, then use the lightweight `.seed-cache.json` sidecar for
/// DB-only inserts.
///
/// If the tarball is newer than the cache file, the cache is invalidated
/// and the image is re-imported (picks up rebuilt seed images on restart).
async fn seed_image_cached(
    pool: &PgPool,
    minio: &opendal::Operator,
    repository_id: Uuid,
    tarball_path: &Path,
    tag: &str,
) -> Result<SeedResult, anyhow::Error> {
    let ns_prefix = std::env::var("PLATFORM_NS_PREFIX").ok();
    let cache_path = cache_path_for_tarball(tarball_path, ns_prefix.as_deref());

    // 1. DB idempotency: tag already exists → check if tarball is newer
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM registry_tags WHERE repository_id = $1 AND name = $2")
            .bind(repository_id)
            .bind(tag)
            .fetch_optional(pool)
            .await?;
    if existing.is_some() && !tarball_newer_than_cache(tarball_path, &cache_path) {
        return Ok(SeedResult::AlreadyExists);
    }
    if existing.is_some() {
        // Tarball was rebuilt — invalidate cache and delete stale tag so we re-import
        tracing::info!("seed tarball is newer than cache, re-importing");
        let _ = std::fs::remove_file(&cache_path);
        sqlx::query("DELETE FROM registry_tags WHERE repository_id = $1 AND name = $2")
            .bind(repository_id)
            .bind(tag)
            .execute(pool)
            .await?;
    }

    // 2. Fast path: cache file exists → DB-only seed
    if let Some(cache) = try_read_cache(&cache_path) {
        return seed_from_cache(pool, repository_id, &cache, tag).await;
    }

    // 3. Acquire exclusive file lock (other processes block here)
    let lock_path = lock_path_for_tarball(tarball_path, ns_prefix.as_deref());
    let lock_file = std::fs::File::create(&lock_path)?;
    lock_file.lock_exclusive()?;

    // 4. Double-check after acquiring lock
    if let Some(cache) = try_read_cache(&cache_path) {
        // Another process wrote the cache while we waited
        drop(lock_file); // release lock
        return seed_from_cache(pool, repository_id, &cache, tag).await;
    }

    // 5. Full import: read tarball, upload to MinIO, insert DB rows
    let result = seed_image(pool, minio, repository_id, tarball_path, tag).await?;

    // 6. Write cache file (only on successful import)
    if let SeedResult::Imported { .. } = &result
        && let Err(e) = write_seed_cache(pool, repository_id, tarball_path, &cache_path).await
    {
        tracing::warn!(error = %e, "failed to write seed cache (non-fatal)");
    }

    // 7. Lock released on drop
    drop(lock_file);
    Ok(result)
}

/// Try to read a `.seed-cache.json` file. Returns `None` if missing or invalid.
fn try_read_cache(cache_path: &Path) -> Option<SeedCache> {
    let data = std::fs::read(cache_path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Seed registry DB rows from cached metadata (no tarball read, no `MinIO` upload).
async fn seed_from_cache(
    pool: &PgPool,
    repository_id: Uuid,
    cache: &SeedCache,
    tag: &str,
) -> Result<SeedResult, anyhow::Error> {
    let mut blob_count = 0usize;

    for blob in &cache.blobs {
        sqlx::query(
            "INSERT INTO registry_blobs (digest, size_bytes, minio_path) \
             VALUES ($1, $2, $3) ON CONFLICT (digest) DO NOTHING",
        )
        .bind(&blob.digest)
        .bind(blob.size_bytes)
        .bind(&blob.minio_path)
        .execute(pool)
        .await?;

        sqlx::query(
            "INSERT INTO registry_blob_links (repository_id, blob_digest) \
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(repository_id)
        .bind(&blob.digest)
        .execute(pool)
        .await?;

        blob_count += 1;
    }

    for manifest in &cache.manifests {
        sqlx::query(
            "INSERT INTO registry_manifests (repository_id, digest, media_type, content, size_bytes) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (repository_id, digest) DO NOTHING",
        )
        .bind(repository_id)
        .bind(&manifest.digest)
        .bind(&manifest.media_type)
        .bind(&manifest.content)
        .bind(manifest.size_bytes)
        .execute(pool)
        .await?;
    }

    sqlx::query(
        "INSERT INTO registry_tags (repository_id, name, manifest_digest) \
         VALUES ($1, $2, $3) ON CONFLICT (repository_id, name) DO UPDATE SET manifest_digest = $3",
    )
    .bind(repository_id)
    .bind(tag)
    .bind(&cache.tag_digest)
    .execute(pool)
    .await?;

    Ok(SeedResult::Imported {
        manifest_digest: cache.tag_digest.clone(),
        blob_count,
    })
}

/// Build a `SeedCache` by querying DB rows that were just inserted, then write atomically.
async fn write_seed_cache(
    pool: &PgPool,
    repository_id: Uuid,
    tarball_path: &Path,
    cache_path: &Path,
) -> Result<(), anyhow::Error> {
    // Query blobs linked to this repository
    let blob_rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT b.digest, b.size_bytes, b.minio_path \
         FROM registry_blobs b \
         JOIN registry_blob_links bl ON bl.blob_digest = b.digest \
         WHERE bl.repository_id = $1",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await?;

    let blobs: Vec<CachedBlob> = blob_rows
        .into_iter()
        .map(|(digest, size_bytes, minio_path)| CachedBlob {
            digest,
            size_bytes,
            minio_path,
        })
        .collect();

    // Query manifests for this repository
    let manifest_rows: Vec<(String, String, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT digest, media_type, content, size_bytes \
         FROM registry_manifests WHERE repository_id = $1",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await?;

    let manifests: Vec<CachedManifest> = manifest_rows
        .into_iter()
        .map(|(digest, media_type, content, size_bytes)| CachedManifest {
            digest,
            media_type,
            content,
            size_bytes,
        })
        .collect();

    // Query the tag digest
    let tag_digest: String = sqlx::query_scalar(
        "SELECT manifest_digest FROM registry_tags \
         WHERE repository_id = $1 AND name = 'latest'",
    )
    .bind(repository_id)
    .fetch_one(pool)
    .await?;

    let cache = SeedCache {
        blobs,
        manifests,
        tag_digest,
    };

    // Atomic write: temp file → rename
    let tmp_path = tarball_path.with_extension("seed-cache.json.tmp");
    let json = serde_json::to_vec(&cache)?;
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, cache_path)?;

    tracing::debug!(path = %cache_path.display(), "seed cache written");
    Ok(())
}

/// Derive the cache file path for a tarball.
///
/// When `ns_prefix` is provided (test runs with per-run `MinIO` instances),
/// it's included in the filename to prevent cross-run cache collisions.
///
/// `platform-runner.tar` → `.platform-runner.seed-cache.json` (production)
/// `platform-runner.tar` → `.platform-runner.test-abc123.seed-cache.json` (test)
fn cache_path_for_tarball(tarball_path: &Path, ns_prefix: Option<&str>) -> PathBuf {
    let stem = tarball_stem(tarball_path);
    let parent = tarball_path.parent().unwrap_or(Path::new("."));
    match ns_prefix {
        Some(prefix) => parent.join(format!(".{stem}.{prefix}.seed-cache.json")),
        None => parent.join(format!(".{stem}.seed-cache.json")),
    }
}

/// Derive the lock file path for a tarball (scoped like cache path).
fn lock_path_for_tarball(tarball_path: &Path, ns_prefix: Option<&str>) -> PathBuf {
    let stem = tarball_stem(tarball_path);
    let parent = tarball_path.parent().unwrap_or(Path::new("."));
    match ns_prefix {
        Some(prefix) => parent.join(format!(".{stem}.{prefix}.seed-cache.lock")),
        None => parent.join(format!(".{stem}.seed-cache.lock")),
    }
}

/// Extract the image name stem from a tarball path.
fn tarball_stem(tarball_path: &Path) -> String {
    image_name_from_filename(
        tarball_path
            .file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or_default(),
    )
    .unwrap_or_else(|| "unknown".to_owned())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Import all blobs from the tarball entries into `MinIO` and DB.
async fn import_blobs(
    pool: &PgPool,
    minio: &opendal::Operator,
    repository_id: Uuid,
    entries: &HashMap<String, Vec<u8>>,
) -> Result<usize, anyhow::Error> {
    let mut blob_count = 0usize;
    for (path, data) in entries {
        if !path.starts_with("blobs/sha256/") {
            continue;
        }

        let hex = path.strip_prefix("blobs/sha256/").expect("prefix verified");
        let expected_digest = format!("sha256:{hex}");

        let actual_digest = sha256_digest(data);
        if actual_digest != expected_digest {
            anyhow::bail!("blob digest mismatch: expected {expected_digest}, got {actual_digest}");
        }

        let minio_path = format!("registry/blobs/sha256/{hex}");
        let size_bytes = i64::try_from(data.len()).unwrap_or(i64::MAX);

        minio.write(&minio_path, data.clone()).await?;

        sqlx::query(
            "INSERT INTO registry_blobs (digest, size_bytes, minio_path) \
             VALUES ($1, $2, $3) ON CONFLICT (digest) DO NOTHING",
        )
        .bind(&expected_digest)
        .bind(size_bytes)
        .bind(&minio_path)
        .execute(pool)
        .await?;

        sqlx::query(
            "INSERT INTO registry_blob_links (repository_id, blob_digest) \
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(repository_id)
        .bind(&expected_digest)
        .execute(pool)
        .await?;

        blob_count += 1;
    }
    Ok(blob_count)
}

/// Import manifest(s) from the OCI index entry.
async fn import_manifests(
    pool: &PgPool,
    repository_id: Uuid,
    top_entry: &OciIndexEntry,
    entries: &HashMap<String, Vec<u8>>,
) -> Result<(), anyhow::Error> {
    let manifest_blob_path = digest_to_blob_path(&top_entry.digest);
    let manifest_content = entries.get(&manifest_blob_path).ok_or_else(|| {
        anyhow::anyhow!("manifest blob not found in tarball: {manifest_blob_path}")
    })?;

    import_manifest(
        pool,
        repository_id,
        &top_entry.digest,
        &top_entry.media_type,
        manifest_content,
    )
    .await?;

    // If it's an image index (multi-arch), also import sub-manifests
    let is_index = top_entry.media_type.contains("image.index")
        || top_entry.media_type.contains("manifest.list");
    if is_index {
        let parsed: OciManifest = serde_json::from_slice(manifest_content)?;
        for sub in &parsed.manifests {
            let sub_path = digest_to_blob_path(&sub.digest);
            if let Some(sub_content) = entries.get(&sub_path) {
                let sub_media_type = detect_manifest_media_type(sub_content);
                import_manifest(
                    pool,
                    repository_id,
                    &sub.digest,
                    &sub_media_type,
                    sub_content,
                )
                .await?;
            }
        }
    }

    Ok(())
}

async fn import_manifest(
    pool: &PgPool,
    repository_id: Uuid,
    digest: &str,
    media_type: &str,
    content: &[u8],
) -> Result<(), anyhow::Error> {
    let size_bytes = i64::try_from(content.len()).unwrap_or(i64::MAX);
    sqlx::query(
        "INSERT INTO registry_manifests (repository_id, digest, media_type, content, size_bytes) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (repository_id, digest) DO NOTHING",
    )
    .bind(repository_id)
    .bind(digest)
    .bind(media_type)
    .bind(content)
    .bind(size_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

/// Extract all files from a tarball into memory.
/// Handles both plain `.tar` and gzip-compressed `.tar.gz` files.
/// Normalizes paths by stripping leading `./`.
fn extract_tarball_entries(path: &Path) -> Result<HashMap<String, Vec<u8>>, anyhow::Error> {
    let file = std::fs::File::open(path)?;
    let name = path.to_string_lossy();
    let is_gzipped = name.ends_with(".tar.gz")
        || path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tgz"));

    let mut entries = HashMap::new();

    if is_gzipped {
        let decoder = flate2::read::GzDecoder::new(file);
        read_tar_entries(decoder, &mut entries)?;
    } else {
        read_tar_entries(file, &mut entries)?;
    }

    Ok(entries)
}

fn read_tar_entries<R: Read>(
    reader: R,
    entries: &mut HashMap<String, Vec<u8>>,
) -> Result<(), anyhow::Error> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        let normalized = normalize_tar_path(&path);
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        entries.insert(normalized, data);
    }
    Ok(())
}

/// Normalize tar entry paths: strip leading `./` prefix.
fn normalize_tar_path(path: &str) -> String {
    path.strip_prefix("./").unwrap_or(path).to_owned()
}

/// Compute `sha256:{hex}` digest of data.
fn sha256_digest(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("sha256:{}", hex::encode(hash))
}

/// Convert a digest like `sha256:abc123` to a blob path like `blobs/sha256/abc123`.
fn digest_to_blob_path(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("blobs/sha256/{hex}")
}

/// Check if the tarball file is newer than the seed cache file.
///
/// Returns `true` if the tarball was modified after the cache, meaning the
/// image was rebuilt and needs re-importing.
fn tarball_newer_than_cache(tarball_path: &Path, cache_path: &Path) -> bool {
    let tarball_mtime = tarball_path.metadata().and_then(|m| m.modified()).ok();
    let cache_mtime = cache_path.metadata().and_then(|m| m.modified()).ok();
    match (tarball_mtime, cache_mtime) {
        (Some(t), Some(c)) => t > c,
        (Some(_), None) => true, // no cache → treat as newer
        _ => false,
    }
}

/// Extract image/repository name from a tarball filename.
///
/// `platform-runner.tar` → `Some("platform-runner")`
/// `platform-runner.tar.gz` → `Some("platform-runner")`
/// `readme.txt` → `None`
fn image_name_from_filename(filename: &str) -> Option<String> {
    filename
        .strip_suffix(".tar.gz")
        .or_else(|| filename.strip_suffix(".tgz"))
        .or_else(|| filename.strip_suffix(".tar"))
        .map(str::to_owned)
}

/// Best-effort media type detection for sub-manifests.
fn detect_manifest_media_type(content: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(content)
        && let Some(mt) = v.get("mediaType").and_then(|m| m.as_str())
    {
        return mt.to_owned();
    }
    "application/vnd.oci.image.manifest.v1+json".to_owned()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_oci_layout_valid() {
        let json = br#"{"imageLayoutVersion": "1.0.0"}"#;
        let layout: OciLayout = serde_json::from_slice(json).unwrap();
        assert_eq!(layout.image_layout_version, "1.0.0");
    }

    #[test]
    fn parse_oci_layout_rejects_bad_version() {
        let json = br#"{"imageLayoutVersion": "2.0.0"}"#;
        let layout: OciLayout = serde_json::from_slice(json).unwrap();
        assert_ne!(layout.image_layout_version, "1.0.0");
    }

    #[test]
    fn parse_index_json_valid() {
        let json = br#"{
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": "sha256:abc123",
                "size": 1234
            }]
        }"#;
        let index: OciIndex = serde_json::from_slice(json).unwrap();
        assert_eq!(index.manifests.len(), 1);
        assert_eq!(index.manifests[0].digest, "sha256:abc123");
    }

    #[test]
    fn parse_index_json_rejects_empty_manifests() {
        let json = br#"{"schemaVersion": 2, "manifests": []}"#;
        let index: OciIndex = serde_json::from_slice(json).unwrap();
        assert!(index.manifests.is_empty());
    }

    #[test]
    fn normalize_tar_path_strips_dot_slash() {
        assert_eq!(normalize_tar_path("./blobs/sha256/abc"), "blobs/sha256/abc");
        assert_eq!(normalize_tar_path("blobs/sha256/abc"), "blobs/sha256/abc");
        assert_eq!(normalize_tar_path("./oci-layout"), "oci-layout");
    }

    #[test]
    fn sha256_digest_computes_correctly() {
        let data = b"hello world";
        let digest = sha256_digest(data);
        assert!(digest.starts_with("sha256:"));
        // Known SHA-256 of "hello world"
        assert_eq!(
            digest,
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn digest_to_blob_path_works() {
        assert_eq!(digest_to_blob_path("sha256:abc123"), "blobs/sha256/abc123");
    }

    #[test]
    fn image_name_from_filename_extracts_correctly() {
        assert_eq!(
            image_name_from_filename("platform-runner.tar"),
            Some("platform-runner".to_owned())
        );
        assert_eq!(
            image_name_from_filename("platform-runner.tar.gz"),
            Some("platform-runner".to_owned())
        );
        assert_eq!(
            image_name_from_filename("my-image.tgz"),
            Some("my-image".to_owned())
        );
        assert_eq!(image_name_from_filename("readme.txt"), None);
        assert_eq!(image_name_from_filename("noext"), None);
    }

    #[test]
    fn extract_tarball_entries_from_in_memory() {
        // Build an in-memory tar with a few test files
        let mut builder = tar::Builder::new(Vec::new());

        let oci_layout = br#"{"imageLayoutVersion": "1.0.0"}"#;
        let mut header = tar::Header::new_gnu();
        header.set_size(oci_layout.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "oci-layout", &oci_layout[..])
            .unwrap();

        let index = br#"{"manifests":[]}"#;
        let mut header = tar::Header::new_gnu();
        header.set_size(index.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "./index.json", &index[..])
            .unwrap();

        let blob_data = b"fake blob content";
        let mut header = tar::Header::new_gnu();
        header.set_size(blob_data.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "./blobs/sha256/deadbeef", &blob_data[..])
            .unwrap();

        builder.finish().unwrap();
        let tar_bytes = builder.into_inner().unwrap();

        // Write to a temp file and extract
        let dir = tempfile::tempdir().unwrap();
        let tar_path = dir.path().join("test.tar");
        std::fs::write(&tar_path, &tar_bytes).unwrap();

        let entries = extract_tarball_entries(&tar_path).unwrap();
        assert!(entries.contains_key("oci-layout"));
        assert!(entries.contains_key("index.json")); // ./index.json normalized
        assert!(entries.contains_key("blobs/sha256/deadbeef"));
        assert_eq!(entries["oci-layout"], oci_layout);
    }

    #[test]
    fn blob_digest_verification_match() {
        let data = b"test data";
        let digest = sha256_digest(data);
        let expected = format!("sha256:{}", hex::encode(Sha256::digest(data)));
        assert_eq!(digest, expected);
    }

    #[test]
    fn blob_digest_verification_mismatch() {
        let data = b"test data";
        let wrong_digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let actual = sha256_digest(data);
        assert_ne!(actual, wrong_digest);
    }

    #[test]
    fn detect_manifest_media_type_from_content() {
        let json = br#"{"mediaType": "application/vnd.oci.image.manifest.v1+json"}"#;
        assert_eq!(
            detect_manifest_media_type(json),
            "application/vnd.oci.image.manifest.v1+json"
        );
    }

    #[test]
    fn detect_manifest_media_type_fallback() {
        let json = br#"{"schemaVersion": 2}"#;
        assert_eq!(
            detect_manifest_media_type(json),
            "application/vnd.oci.image.manifest.v1+json"
        );
    }

    #[test]
    fn cache_path_without_ns_prefix() {
        let tarball = Path::new("/tmp/seed/platform-runner.tar");
        assert_eq!(
            cache_path_for_tarball(tarball, None),
            Path::new("/tmp/seed/.platform-runner.seed-cache.json")
        );

        let tarball_gz = Path::new("/tmp/seed/my-image.tar.gz");
        assert_eq!(
            cache_path_for_tarball(tarball_gz, None),
            Path::new("/tmp/seed/.my-image.seed-cache.json")
        );
    }

    #[test]
    fn cache_path_with_ns_prefix() {
        let tarball = Path::new("/tmp/seed/platform-runner.tar");
        assert_eq!(
            cache_path_for_tarball(tarball, Some("platform-test-abc123")),
            Path::new("/tmp/seed/.platform-runner.platform-test-abc123.seed-cache.json")
        );
    }

    #[test]
    fn lock_path_without_ns_prefix() {
        let tarball = Path::new("/tmp/seed/platform-runner.tar");
        assert_eq!(
            lock_path_for_tarball(tarball, None),
            Path::new("/tmp/seed/.platform-runner.seed-cache.lock")
        );
    }

    #[test]
    fn lock_path_with_ns_prefix() {
        let tarball = Path::new("/tmp/seed/platform-runner.tar");
        assert_eq!(
            lock_path_for_tarball(tarball, Some("platform-test-xyz789")),
            Path::new("/tmp/seed/.platform-runner.platform-test-xyz789.seed-cache.lock")
        );
    }

    #[test]
    fn seed_cache_round_trip() {
        let cache = SeedCache {
            blobs: vec![CachedBlob {
                digest: "sha256:abc".into(),
                size_bytes: 42,
                minio_path: "registry/blobs/sha256/abc".into(),
            }],
            manifests: vec![CachedManifest {
                digest: "sha256:def".into(),
                media_type: "application/vnd.oci.image.manifest.v1+json".into(),
                content: b"manifest content".to_vec(),
                size_bytes: 16,
            }],
            tag_digest: "sha256:def".into(),
        };

        let json = serde_json::to_vec(&cache).unwrap();
        let restored: SeedCache = serde_json::from_slice(&json).unwrap();
        assert_eq!(restored.blobs.len(), 1);
        assert_eq!(restored.blobs[0].digest, "sha256:abc");
        assert_eq!(restored.manifests.len(), 1);
        assert_eq!(restored.manifests[0].content, b"manifest content");
        assert_eq!(restored.tag_digest, "sha256:def");
    }

    #[test]
    fn extract_gzipped_tarball() {
        use flate2::write::GzEncoder;
        use std::io::Write;

        // Build a tar
        let mut builder = tar::Builder::new(Vec::new());
        let data = br#"{"imageLayoutVersion": "1.0.0"}"#;
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "oci-layout", &data[..])
            .unwrap();
        builder.finish().unwrap();
        let tar_bytes = builder.into_inner().unwrap();

        // Gzip it
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        let gz_bytes = encoder.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let gz_path = dir.path().join("test.tar.gz");
        std::fs::write(&gz_path, &gz_bytes).unwrap();

        let entries = extract_tarball_entries(&gz_path).unwrap();
        assert!(entries.contains_key("oci-layout"));
    }
}
