use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::store::AppState;

/// Query params for agent-runner download.
#[derive(Debug, serde::Deserialize)]
pub struct DownloadParams {
    /// Target architecture: `amd64`, `x86_64`, `arm64`, or `aarch64`.
    pub arch: String,
}

/// Normalize architecture strings to canonical names (amd64, arm64).
fn normalize_arch(arch: &str) -> Result<&'static str, ApiError> {
    match arch {
        "amd64" | "x86_64" => Ok("amd64"),
        "arm64" | "aarch64" => Ok("arm64"),
        _ => Err(ApiError::BadRequest(
            "arch must be 'amd64' or 'arm64'".into(),
        )),
    }
}

/// `GET /api/downloads/agent-runner?arch=amd64`
///
/// Serves the cross-compiled agent-runner binary for the requested architecture.
/// Auth: Bearer token (agent pods have `PLATFORM_API_TOKEN`).
#[tracing::instrument(skip(state, _auth), fields(arch = %params.arch), err)]
async fn download_agent_runner(
    State(state): State<AppState>,
    _auth: AuthUser,
    Query(params): Query<DownloadParams>,
) -> Result<Response, ApiError> {
    let arch = normalize_arch(&params.arch)?;
    let binary_path = state.config.agent_runner_dir.join(arch);

    let data = tokio::fs::read(&binary_path).await.map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "agent-runner binary not found for {arch}: {e}"
        ))
    })?;

    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        "content-disposition",
        HeaderValue::from_static("attachment; filename=\"agent-runner\""),
    );
    headers.insert(
        "cache-control",
        HeaderValue::from_static("public, max-age=3600"),
    );

    Ok((StatusCode::OK, headers, data).into_response())
}

/// `GET /api/downloads/mcp-servers`
///
/// Serves a pre-built tarball of the MCP servers directory (`mcp/`).
/// Agent pods extract this at startup so they always get the latest MCP tools
/// without rebuilding the Docker image.
#[tracing::instrument(skip(state, _auth), err)]
async fn download_mcp_servers(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Response, ApiError> {
    let tarball_path = &state.config.mcp_servers_tarball;

    let data = tokio::fs::read(tarball_path).await.map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "MCP servers tarball not found at {}: {e}",
            tarball_path.display()
        ))
    })?;

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/gzip"));
    headers.insert(
        "content-disposition",
        HeaderValue::from_static("attachment; filename=\"mcp-servers.tar.gz\""),
    );
    headers.insert(
        "cache-control",
        HeaderValue::from_static("public, max-age=3600"),
    );

    Ok((StatusCode::OK, headers, data).into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/downloads/agent-runner",
            axum::routing::get(download_agent_runner),
        )
        .route(
            "/api/downloads/mcp-servers",
            axum::routing::get(download_mcp_servers),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_arch_amd64() {
        assert_eq!(normalize_arch("amd64").unwrap(), "amd64");
    }

    #[test]
    fn normalize_arch_x86_64() {
        assert_eq!(normalize_arch("x86_64").unwrap(), "amd64");
    }

    #[test]
    fn normalize_arch_arm64() {
        assert_eq!(normalize_arch("arm64").unwrap(), "arm64");
    }

    #[test]
    fn normalize_arch_aarch64() {
        assert_eq!(normalize_arch("aarch64").unwrap(), "arm64");
    }

    #[test]
    fn normalize_arch_invalid() {
        assert!(normalize_arch("ppc64").is_err());
    }

    #[test]
    fn normalize_arch_empty() {
        assert!(normalize_arch("").is_err());
    }
}
