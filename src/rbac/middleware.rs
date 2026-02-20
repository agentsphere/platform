use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::resolver;
use crate::rbac::types::Permission;
use crate::store::AppState;

/// Create a middleware layer that enforces a required permission.
///
/// Usage:
/// ```ignore
/// Router::new()
///     .route("/endpoint", get(handler))
///     .route_layer(axum::middleware::from_fn_with_state(
///         state.clone(),
///         require_permission(Permission::ProjectRead),
///     ))
/// ```
///
/// For project-scoped permissions, extracts `project_id` from the URL path.
/// For global permissions (admin routes), checks without project scope.
#[allow(dead_code)] // used as route_layer in modules 03-09
#[allow(clippy::type_complexity)]
pub fn require_permission(
    perm: Permission,
) -> impl Fn(
    State<AppState>,
    AuthUser,
    Request,
    Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, ApiError>> + Send>>
+ Clone
+ Send {
    move |State(state): State<AppState>, auth: AuthUser, req: Request, next: Next| {
        Box::pin(async move {
            // Try to extract project_id from path
            let project_id = extract_project_id_from_path(&req);

            let allowed = resolver::has_permission_scoped(
                &state.pool,
                &state.valkey,
                auth.user_id,
                project_id,
                perm,
                auth.token_scopes.as_deref(),
            )
            .await
            .map_err(ApiError::Internal)?;

            if !allowed {
                tracing::warn!(
                    user_id = %auth.user_id,
                    permission = %perm,
                    "permission denied"
                );
                return Err(ApiError::Forbidden);
            }

            Ok(next.run(req).await)
        })
    }
}

/// Extract `project_id` UUID from the URL path parameters.
/// Looks for a segment after `/projects/` in the path.
fn extract_project_id_from_path(req: &Request) -> Option<Uuid> {
    let path = req.uri().path();
    let segments: Vec<&str> = path.split('/').collect();

    // Look for /projects/:id or /api/projects/:id pattern
    for window in segments.windows(2) {
        if window[0] == "projects"
            && let Ok(id) = window[1].parse::<Uuid>()
        {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn req_with_path(path: &str) -> Request {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    #[test]
    fn project_id_from_api_path() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let req = req_with_path(&format!("/api/projects/{id}/issues"));
        let result = extract_project_id_from_path(&req);
        assert_eq!(result, Some(id.parse().unwrap()));
    }

    #[test]
    fn project_id_from_root_path() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let req = req_with_path(&format!("/projects/{id}"));
        assert_eq!(
            extract_project_id_from_path(&req),
            Some(id.parse().unwrap())
        );
    }

    #[test]
    fn project_id_not_present() {
        let req = req_with_path("/api/users/123");
        assert_eq!(extract_project_id_from_path(&req), None);
    }

    #[test]
    fn project_id_invalid_uuid() {
        let req = req_with_path("/api/projects/not-a-uuid/issues");
        assert_eq!(extract_project_id_from_path(&req), None);
    }

    #[test]
    fn project_id_trailing_slash() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let req = req_with_path(&format!("/api/projects/{id}/"));
        assert_eq!(
            extract_project_id_from_path(&req),
            Some(id.parse().unwrap())
        );
    }

    #[test]
    fn project_id_from_nested_path() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let req = req_with_path(&format!("/api/projects/{id}/pipelines/abc"));
        assert_eq!(
            extract_project_id_from_path(&req),
            Some(id.parse().unwrap())
        );
    }
}
