//! SPIFFE identity types for service mesh.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::mesh::error::MeshError;

/// A SPIFFE identity in the form `spiffe://platform/{namespace}/{service}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpiffeId {
    namespace: String,
    service: String,
}

impl SpiffeId {
    /// Create a new SPIFFE identity, validating namespace and service components.
    ///
    /// Both must pass `check_name()` validation (1-255 chars, alphanumeric + `-_.`).
    pub fn new(namespace: &str, service: &str) -> Result<Self, MeshError> {
        validate_component("namespace", namespace)?;
        validate_component("service", service)?;
        Ok(Self {
            namespace: namespace.to_owned(),
            service: service.to_owned(),
        })
    }

    /// Return the full SPIFFE URI.
    pub fn uri(&self) -> String {
        format!("spiffe://platform/{}/{}", self.namespace, self.service)
    }

    /// Return the namespace component.
    #[allow(dead_code)]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Return the service component.
    #[allow(dead_code)]
    pub fn service(&self) -> &str {
        &self.service
    }
}

impl fmt::Display for SpiffeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.uri())
    }
}

/// Validate a SPIFFE identity component using the same rules as `check_name`.
fn validate_component(field: &str, value: &str) -> Result<(), MeshError> {
    // Re-use validation::check_name logic but map errors to MeshError
    crate::validation::check_name(value).map_err(|e| match e {
        ApiError::BadRequest(msg) => MeshError::InvalidSpiffeId(format!("{field}: {msg}")),
        _ => MeshError::InvalidSpiffeId(format!("{field}: invalid")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_spiffe_id() {
        let id = SpiffeId::new("default", "my-service").unwrap();
        assert_eq!(id.uri(), "spiffe://platform/default/my-service");
        assert_eq!(id.namespace(), "default");
        assert_eq!(id.service(), "my-service");
    }

    #[test]
    fn display_format() {
        let id = SpiffeId::new("prod", "api-gateway").unwrap();
        assert_eq!(id.to_string(), "spiffe://platform/prod/api-gateway");
    }

    #[test]
    fn rejects_empty_namespace() {
        let err = SpiffeId::new("", "svc").unwrap_err();
        assert!(matches!(err, MeshError::InvalidSpiffeId(_)));
    }

    #[test]
    fn rejects_empty_service() {
        let err = SpiffeId::new("ns", "").unwrap_err();
        assert!(matches!(err, MeshError::InvalidSpiffeId(_)));
    }

    #[test]
    fn rejects_slashes_in_namespace() {
        let err = SpiffeId::new("ns/bad", "svc").unwrap_err();
        assert!(matches!(err, MeshError::InvalidSpiffeId(_)));
    }

    #[test]
    fn rejects_spaces_in_service() {
        let err = SpiffeId::new("ns", "my service").unwrap_err();
        assert!(matches!(err, MeshError::InvalidSpiffeId(_)));
    }

    #[test]
    fn allows_dots_underscores_hyphens() {
        let id = SpiffeId::new("my_ns.v1", "api-gw_v2.0").unwrap();
        assert_eq!(id.uri(), "spiffe://platform/my_ns.v1/api-gw_v2.0");
    }

    #[test]
    fn rejects_too_long_namespace() {
        let long = "a".repeat(256);
        let err = SpiffeId::new(&long, "svc").unwrap_err();
        assert!(matches!(err, MeshError::InvalidSpiffeId(_)));
    }

    #[test]
    fn serialization_roundtrip() {
        let id = SpiffeId::new("ns", "svc").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        let decoded: SpiffeId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }
}
