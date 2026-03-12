use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// OCI Distribution Spec error codes.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // All codes defined per OCI spec; not all used yet
pub enum OciErrorCode {
    BlobUnknown,
    BlobUploadInvalid,
    BlobUploadUnknown,
    DigestInvalid,
    ManifestInvalid,
    ManifestUnknown,
    NameInvalid,
    NameUnknown,
    SizeInvalid,
    Unauthorized,
    Denied,
    Unsupported,
}

impl OciErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlobUnknown => "BLOB_UNKNOWN",
            Self::BlobUploadInvalid => "BLOB_UPLOAD_INVALID",
            Self::BlobUploadUnknown => "BLOB_UPLOAD_UNKNOWN",
            Self::DigestInvalid => "DIGEST_INVALID",
            Self::ManifestInvalid => "MANIFEST_INVALID",
            Self::ManifestUnknown => "MANIFEST_UNKNOWN",
            Self::NameInvalid => "NAME_INVALID",
            Self::NameUnknown => "NAME_UNKNOWN",
            Self::SizeInvalid => "SIZE_INVALID",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Denied => "DENIED",
            Self::Unsupported => "UNSUPPORTED",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            Self::BlobUnknown
            | Self::BlobUploadUnknown
            | Self::ManifestUnknown
            | Self::NameUnknown => StatusCode::NOT_FOUND,
            Self::BlobUploadInvalid
            | Self::DigestInvalid
            | Self::ManifestInvalid
            | Self::NameInvalid
            | Self::SizeInvalid => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Denied => StatusCode::FORBIDDEN,
            Self::Unsupported => StatusCode::METHOD_NOT_ALLOWED,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // All variants defined per OCI spec; not all constructed yet
pub enum RegistryError {
    #[error("blob unknown")]
    BlobUnknown,
    #[error("blob upload invalid: {0}")]
    BlobUploadInvalid(String),
    #[error("blob upload unknown")]
    BlobUploadUnknown,
    #[error("digest invalid: {0}")]
    DigestInvalid(String),
    #[error("manifest invalid: {0}")]
    ManifestInvalid(String),
    #[error("manifest unknown")]
    ManifestUnknown,
    #[error("name unknown")]
    NameUnknown,
    #[error("unauthorized")]
    Unauthorized,
    #[error("denied")]
    Denied,
    #[error("tag already exists: {0}")]
    TagExists(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] opendal::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl RegistryError {
    fn oci_code(&self) -> OciErrorCode {
        match self {
            Self::BlobUnknown | Self::Db(_) | Self::Storage(_) | Self::Internal(_) => {
                OciErrorCode::BlobUnknown // generic 404 to not leak internals
            }
            Self::BlobUploadInvalid(_) => OciErrorCode::BlobUploadInvalid,
            Self::BlobUploadUnknown => OciErrorCode::BlobUploadUnknown,
            Self::DigestInvalid(_) => OciErrorCode::DigestInvalid,
            Self::ManifestInvalid(_) => OciErrorCode::ManifestInvalid,
            Self::ManifestUnknown => OciErrorCode::ManifestUnknown,
            Self::NameUnknown => OciErrorCode::NameUnknown,
            Self::Unauthorized => OciErrorCode::Unauthorized,
            Self::Denied | Self::TagExists(_) => OciErrorCode::Denied,
        }
    }
}

impl IntoResponse for RegistryError {
    fn into_response(self) -> Response {
        let code = self.oci_code();

        // Internal errors get logged but not exposed
        let message = match &self {
            Self::Db(e) => {
                tracing::error!(error = %e, "registry database error");
                "internal error".to_string()
            }
            Self::Storage(e) => {
                tracing::error!(error = %e, "registry storage error");
                "internal error".to_string()
            }
            Self::Internal(e) => {
                tracing::error!(error = %e, "registry internal error");
                "internal error".to_string()
            }
            other => other.to_string(),
        };

        // For DB/storage/internal errors, use 500 instead of the OCI code
        let status = match &self {
            Self::Db(_) | Self::Storage(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TagExists(_) => StatusCode::CONFLICT,
            _ => code.status(),
        };

        let body = serde_json::json!({
            "errors": [{
                "code": code.as_str(),
                "message": message,
                "detail": {}
            }]
        });

        // 401 responses must include Www-Authenticate per OCI spec so that
        // containerd/Docker know to retry with credentials from imagePullSecrets.
        if status == StatusCode::UNAUTHORIZED {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "www-authenticate",
                HeaderValue::from_static(r#"Basic realm="platform-registry""#),
            );
            return (status, headers, axum::Json(body)).into_response();
        }

        (status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_error_code_as_str() {
        assert_eq!(OciErrorCode::BlobUnknown.as_str(), "BLOB_UNKNOWN");
        assert_eq!(OciErrorCode::Unauthorized.as_str(), "UNAUTHORIZED");
        assert_eq!(OciErrorCode::DigestInvalid.as_str(), "DIGEST_INVALID");
    }

    #[test]
    fn oci_error_status_codes() {
        assert_eq!(OciErrorCode::BlobUnknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            OciErrorCode::DigestInvalid.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            OciErrorCode::Unauthorized.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(OciErrorCode::Denied.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn registry_error_to_oci_code() {
        assert!(matches!(
            RegistryError::BlobUnknown.oci_code(),
            OciErrorCode::BlobUnknown
        ));
        assert!(matches!(
            RegistryError::ManifestUnknown.oci_code(),
            OciErrorCode::ManifestUnknown
        ));
        assert!(matches!(
            RegistryError::Unauthorized.oci_code(),
            OciErrorCode::Unauthorized
        ));
    }

    #[test]
    fn error_display() {
        assert_eq!(RegistryError::BlobUnknown.to_string(), "blob unknown");
        assert_eq!(
            RegistryError::DigestInvalid("bad".into()).to_string(),
            "digest invalid: bad"
        );
    }
}
