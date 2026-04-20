// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! OCI tag listing handler.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use platform_registry::{RegistryError, TagListResponse};

use super::auth::OptionalRegistryUser;
use crate::state::PlatformState;

#[derive(Debug, Deserialize)]
pub struct TagListQuery {
    /// Maximum number of tags to return.
    pub n: Option<i64>,
    /// Cursor: return tags lexicographically after this value.
    pub last: Option<String>,
}

// ---------------------------------------------------------------------------
// GET /v2/{name}/tags/list
// ---------------------------------------------------------------------------

pub async fn list_tags(
    State(state): State<PlatformState>,
    OptionalRegistryUser(user): OptionalRegistryUser,
    Path(name): Path<String>,
    Query(query): Query<TagListQuery>,
) -> Result<Json<TagListResponse>, RegistryError> {
    let access = super::resolve_optional_access(&state, user.as_ref(), &name, false).await?;

    let limit = query.n.unwrap_or(100).min(1000);

    let tags = if let Some(ref last) = query.last {
        sqlx::query_scalar!(
            r#"SELECT name FROM registry_tags
               WHERE repository_id = $1 AND name > $2
               ORDER BY name
               LIMIT $3"#,
            access.repository_id,
            last,
            limit,
        )
        .fetch_all(&state.pool)
        .await?
    } else {
        sqlx::query_scalar!(
            r#"SELECT name FROM registry_tags
               WHERE repository_id = $1
               ORDER BY name
               LIMIT $2"#,
            access.repository_id,
            limit,
        )
        .fetch_all(&state.pool)
        .await?
    };

    Ok(Json(TagListResponse { name, tags }))
}

// ---------------------------------------------------------------------------
// Namespaced wrapper (two-segment: {ns}/{repo})
// ---------------------------------------------------------------------------

pub async fn list_tags_ns(
    state: State<PlatformState>,
    user: OptionalRegistryUser,
    Path((ns, repo)): Path<(String, String)>,
    query: Query<TagListQuery>,
) -> Result<Json<TagListResponse>, RegistryError> {
    list_tags(state, user, Path(format!("{ns}/{repo}")), query).await
}
