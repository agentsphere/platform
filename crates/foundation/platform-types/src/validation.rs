// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use crate::error::ApiError;

/// Check that a string field length is within [min, max].
pub fn check_length(field: &str, value: &str, min: usize, max: usize) -> Result<(), ApiError> {
    let len = value.len();
    if len < min || len > max {
        return Err(ApiError::BadRequest(format!(
            "{field} must be between {min} and {max} characters (got {len})"
        )));
    }
    Ok(())
}

const GIT_UNSAFE: &[char] = &['~', '^', ':', '*', '[', '?', '\\'];

/// Validates a git branch name.
///
/// Rules: 1-255 chars, no `..`, no null bytes, no git-unsafe characters.
pub fn check_branch_name(value: &str) -> Result<(), ApiError> {
    check_length("branch name", value, 1, 255)?;
    if value.contains("..") || value.contains('\0') {
        return Err(ApiError::BadRequest(
            "branch name must not contain '..' or null bytes".into(),
        ));
    }
    if value.contains(GIT_UNSAFE) {
        return Err(ApiError::BadRequest(
            "branch name contains unsafe characters".into(),
        ));
    }
    Ok(())
}

/// Validates a container image reference for use in pipeline steps.
///
/// Like `check_container_image` but allows `$` for Kubernetes env var substitution
/// (e.g. `${CI_REGISTRY}/app:${VERSION}`).
pub fn check_pipeline_image(image: &str) -> Result<(), ApiError> {
    check_length("image", image, 1, 500)?;

    let forbidden = [';', '&', '|', '`', '\'', '"', '\\', '\n', '\r', ' ', '\t'];
    if image.chars().any(|c| forbidden.contains(&c)) {
        return Err(ApiError::BadRequest(
            "image: contains forbidden characters".into(),
        ));
    }

    if !image.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::BadRequest(
            "image: must contain alphanumeric characters".into(),
        ));
    }

    Ok(())
}

/// Simple glob-like pattern matching for branch/tag names.
///
/// Supports `*` as a wildcard matching any sequence of characters.
/// Used by branch protection rules and pipeline trigger matching.
pub fn match_glob_pattern(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if !pattern.contains('*') {
        return pattern == value;
    }

    let segments: Vec<&str> = pattern.split('*').collect();

    // First segment must be a prefix of value
    let prefix = segments[0];
    if !value.starts_with(prefix) {
        return false;
    }

    // Last segment must be a suffix of the remaining string
    let suffix = segments[segments.len() - 1];
    // Check that there's enough room for prefix + suffix (handles overlap edge case)
    if value.len() < prefix.len() + suffix.len() {
        return false;
    }
    if !value.ends_with(suffix) {
        return false;
    }

    // Walk middle segments in order, each must be found after the previous match
    let mut cursor = prefix.len();
    let end = value.len() - suffix.len();
    for &seg in &segments[1..segments.len() - 1] {
        if let Some(pos) = value[cursor..end].find(seg) {
            cursor += pos + seg.len();
        } else {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_length_ok() {
        assert!(check_length("name", "hello", 1, 10).is_ok());
    }

    #[test]
    fn check_length_too_short() {
        assert!(check_length("name", "", 1, 10).is_err());
    }

    #[test]
    fn check_length_too_long() {
        assert!(check_length("name", "hello world!", 1, 5).is_err());
    }

    #[test]
    fn check_length_exact_boundary() {
        assert!(check_length("name", "12345", 5, 5).is_ok());
    }

    // --- check_pipeline_image ---

    #[test]
    fn pipeline_image_valid() {
        assert!(check_pipeline_image("nginx:latest").is_ok());
        assert!(check_pipeline_image("registry.io/app:v1.0").is_ok());
        assert!(check_pipeline_image("${CI_REGISTRY}/app:${VERSION}").is_ok());
    }

    #[test]
    fn pipeline_image_empty() {
        assert!(check_pipeline_image("").is_err());
    }

    #[test]
    fn pipeline_image_forbidden_chars() {
        assert!(check_pipeline_image("nginx;rm -rf /").is_err());
        assert!(check_pipeline_image("nginx&bg").is_err());
        assert!(check_pipeline_image("nginx|pipe").is_err());
        assert!(check_pipeline_image("nginx`cmd`").is_err());
        assert!(check_pipeline_image("nginx image").is_err());
    }

    #[test]
    fn pipeline_image_no_alphanumeric() {
        assert!(check_pipeline_image("${}:.-_/").is_err());
    }

    // --- match_glob_pattern ---

    #[test]
    fn glob_star_matches_all() {
        assert!(match_glob_pattern("*", "anything"));
        assert!(match_glob_pattern("*", ""));
    }

    #[test]
    fn glob_exact_match() {
        assert!(match_glob_pattern("main", "main"));
        assert!(!match_glob_pattern("main", "develop"));
    }

    #[test]
    fn glob_prefix_wildcard() {
        assert!(match_glob_pattern("feature/*", "feature/login"));
        assert!(!match_glob_pattern("feature/*", "hotfix/login"));
    }

    #[test]
    fn glob_suffix_wildcard() {
        assert!(match_glob_pattern("*-prod", "release-prod"));
        assert!(!match_glob_pattern("*-prod", "release-staging"));
    }

    #[test]
    fn glob_middle_wildcard() {
        assert!(match_glob_pattern("v*-rc", "v1.0-rc"));
        assert!(!match_glob_pattern("v*-rc", "v1.0-beta"));
    }

    #[test]
    fn glob_multiple_wildcards() {
        assert!(match_glob_pattern("release/*-*", "release/v1-rc"));
        assert!(!match_glob_pattern("release/*-*", "hotfix/v1-rc"));
    }
}
