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

    // -- All OciErrorCode variants --

    #[test]
    fn oci_error_code_all_as_str() {
        assert_eq!(OciErrorCode::BlobUnknown.as_str(), "BLOB_UNKNOWN");
        assert_eq!(
            OciErrorCode::BlobUploadInvalid.as_str(),
            "BLOB_UPLOAD_INVALID"
        );
        assert_eq!(
            OciErrorCode::BlobUploadUnknown.as_str(),
            "BLOB_UPLOAD_UNKNOWN"
        );
        assert_eq!(OciErrorCode::DigestInvalid.as_str(), "DIGEST_INVALID");
        assert_eq!(OciErrorCode::ManifestInvalid.as_str(), "MANIFEST_INVALID");
        assert_eq!(OciErrorCode::ManifestUnknown.as_str(), "MANIFEST_UNKNOWN");
        assert_eq!(OciErrorCode::NameInvalid.as_str(), "NAME_INVALID");
        assert_eq!(OciErrorCode::NameUnknown.as_str(), "NAME_UNKNOWN");
        assert_eq!(OciErrorCode::SizeInvalid.as_str(), "SIZE_INVALID");
        assert_eq!(OciErrorCode::Unauthorized.as_str(), "UNAUTHORIZED");
        assert_eq!(OciErrorCode::Denied.as_str(), "DENIED");
        assert_eq!(OciErrorCode::Unsupported.as_str(), "UNSUPPORTED");
    }

    // -- All OciErrorCode status codes --

    #[test]
    fn oci_error_code_all_status_codes() {
        // 404 group
        assert_eq!(OciErrorCode::BlobUnknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            OciErrorCode::BlobUploadUnknown.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            OciErrorCode::ManifestUnknown.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(OciErrorCode::NameUnknown.status(), StatusCode::NOT_FOUND);

        // 400 group
        assert_eq!(
            OciErrorCode::BlobUploadInvalid.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            OciErrorCode::DigestInvalid.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            OciErrorCode::ManifestInvalid.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(OciErrorCode::NameInvalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(OciErrorCode::SizeInvalid.status(), StatusCode::BAD_REQUEST);

        // Others
        assert_eq!(
            OciErrorCode::Unauthorized.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(OciErrorCode::Denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            OciErrorCode::Unsupported.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    // -- All RegistryError → OciCode mappings --

    #[test]
    fn registry_error_all_oci_code_mappings() {
        assert!(matches!(
            RegistryError::BlobUnknown.oci_code(),
            OciErrorCode::BlobUnknown
        ));
        assert!(matches!(
            RegistryError::BlobUploadInvalid("x".into()).oci_code(),
            OciErrorCode::BlobUploadInvalid
        ));
        assert!(matches!(
            RegistryError::BlobUploadUnknown.oci_code(),
            OciErrorCode::BlobUploadUnknown
        ));
        assert!(matches!(
            RegistryError::DigestInvalid("x".into()).oci_code(),
            OciErrorCode::DigestInvalid
        ));
        assert!(matches!(
            RegistryError::ManifestInvalid("x".into()).oci_code(),
            OciErrorCode::ManifestInvalid
        ));
        assert!(matches!(
            RegistryError::ManifestUnknown.oci_code(),
            OciErrorCode::ManifestUnknown
        ));
        assert!(matches!(
            RegistryError::NameUnknown.oci_code(),
            OciErrorCode::NameUnknown
        ));
        assert!(matches!(
            RegistryError::Unauthorized.oci_code(),
            OciErrorCode::Unauthorized
        ));
        assert!(matches!(
            RegistryError::Denied.oci_code(),
            OciErrorCode::Denied
        ));
        assert!(matches!(
            RegistryError::TagExists("x".into()).oci_code(),
            OciErrorCode::Denied
        ));
    }

    // -- IntoResponse: JSON structure --

    #[tokio::test]
    async fn registry_error_response_json_structure() {
        use axum::body::to_bytes;

        let err = RegistryError::ManifestUnknown;
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let bytes = to_bytes(response.into_body(), 10_000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["errors"].is_array());
        let err_obj = &json["errors"][0];
        assert_eq!(err_obj["code"], "MANIFEST_UNKNOWN");
        assert_eq!(err_obj["message"], "manifest unknown");
        assert!(err_obj["detail"].is_object());
    }

    // -- IntoResponse: 401 includes Www-Authenticate header --

    #[test]
    fn registry_error_unauthorized_includes_www_authenticate() {
        let err = RegistryError::Unauthorized;
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let www_auth = response
            .headers()
            .get("www-authenticate")
            .expect("401 response must include Www-Authenticate");
        assert_eq!(
            www_auth.to_str().unwrap(),
            r#"Basic realm="platform-registry""#
        );
    }

    // -- IntoResponse: internal errors use 500 --

    #[test]
    fn registry_error_internal_uses_500() {
        let err = RegistryError::Internal(anyhow::anyhow!("boom"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn registry_error_storage_uses_500() {
        // Create a dummy opendal error
        let opendal_err = opendal::Error::new(opendal::ErrorKind::Unexpected, "test");
        let err = RegistryError::Storage(opendal_err);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // -- IntoResponse: TagExists uses 409 --

    #[test]
    fn registry_error_tag_exists_uses_409() {
        let err = RegistryError::TagExists("v1".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    // -- IntoResponse: normal errors don't expose messages for internal --

    #[tokio::test]
    async fn registry_error_internal_does_not_leak_details() {
        use axum::body::to_bytes;

        let err = RegistryError::Internal(anyhow::anyhow!("secret database error"));
        let response = err.into_response();
        let bytes = to_bytes(response.into_body(), 10_000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["errors"][0]["message"], "internal error",
            "internal error details should not be leaked"
        );
    }

    // -- All display strings --

    #[test]
    fn error_display_all_variants() {
        assert_eq!(RegistryError::BlobUnknown.to_string(), "blob unknown");
        assert_eq!(
            RegistryError::BlobUploadInvalid("reason".into()).to_string(),
            "blob upload invalid: reason"
        );
        assert_eq!(
            RegistryError::BlobUploadUnknown.to_string(),
            "blob upload unknown"
        );
        assert_eq!(
            RegistryError::DigestInvalid("bad".into()).to_string(),
            "digest invalid: bad"
        );
        assert_eq!(
            RegistryError::ManifestInvalid("x".into()).to_string(),
            "manifest invalid: x"
        );
        assert_eq!(
            RegistryError::ManifestUnknown.to_string(),
            "manifest unknown"
        );
        assert_eq!(RegistryError::NameUnknown.to_string(), "name unknown");
        assert_eq!(RegistryError::Unauthorized.to_string(), "unauthorized");
        assert_eq!(RegistryError::Denied.to_string(), "denied");
        assert_eq!(
            RegistryError::TagExists("v1".into()).to_string(),
            "tag already exists: v1"
        );
    }

    // -- IntoResponse: BlobUploadInvalid returns correct status --

    #[test]
    fn registry_error_blob_upload_invalid_returns_400() {
        let err = RegistryError::BlobUploadInvalid("too large".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn registry_error_digest_invalid_returns_400() {
        let err = RegistryError::DigestInvalid("wrong hash".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn registry_error_denied_returns_403() {
        let err = RegistryError::Denied;
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn registry_error_name_unknown_returns_404() {
        let err = RegistryError::NameUnknown;
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn registry_error_blob_upload_unknown_returns_404() {
        let err = RegistryError::BlobUploadUnknown;
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
