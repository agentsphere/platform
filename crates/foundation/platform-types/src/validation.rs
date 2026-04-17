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

/// Validates a name field: 1-255 chars, alphanumeric + hyphens/underscores/dots,
/// no leading/trailing dot.
pub fn check_name(value: &str) -> Result<(), ApiError> {
    check_length("name", value, 1, 255)?;
    if value.starts_with('.') || value.ends_with('.') {
        return Err(ApiError::BadRequest(
            "name: must not start or end with a dot".into(),
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ApiError::BadRequest(
            "name must contain only alphanumeric characters, hyphens, underscores, or dots".into(),
        ));
    }
    Ok(())
}

/// Validates an email address: 3-254 chars, exactly one `@` with non-empty parts.
pub fn check_email(value: &str) -> Result<(), ApiError> {
    check_length("email", value, 3, 254)?;
    let parts: Vec<&str> = value.splitn(3, '@').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(ApiError::BadRequest(
            "email: must have exactly one @ with non-empty local and domain parts".into(),
        ));
    }
    Ok(())
}

/// Validates a list of labels: max 50, each 1-100 chars.
pub fn check_labels(labels: &[String]) -> Result<(), ApiError> {
    if labels.len() > 50 {
        return Err(ApiError::BadRequest("max 50 labels".into()));
    }
    for label in labels {
        check_length("label", label, 1, 100)?;
    }
    Ok(())
}

/// Validates a URL: 1-2048 chars, http(s) only.
pub fn check_url(value: &str) -> Result<(), ApiError> {
    check_length("url", value, 1, 2048)?;
    if !value.starts_with("http://") && !value.starts_with("https://") {
        return Err(ApiError::BadRequest(
            "url must use http or https scheme".into(),
        ));
    }
    Ok(())
}

/// Validates an LFS object ID: exactly 64 hex chars.
pub fn check_lfs_oid(oid: &str) -> Result<(), ApiError> {
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest(
            "LFS OID must be exactly 64 hex characters".into(),
        ));
    }
    Ok(())
}

/// Check whether an IP address is private/reserved (SSRF protection).
pub fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_private_ipv4(v4),
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped_v4) = v6.to_ipv4_mapped() {
                return is_private_ipv4(mapped_v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || is_ipv6_unique_local(&v6)
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn is_private_ipv4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
}

fn is_ipv6_unique_local(v6: &std::net::Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// Validate a URL against SSRF attacks.
///
/// Blocks private/loopback IPs, link-local, metadata endpoints,
/// non-allowed schemes, and DNS names resolving to private IPs.
pub fn check_ssrf_url(url_str: &str, allowed_schemes: &[&str]) -> Result<(), ApiError> {
    use std::net::ToSocketAddrs;

    let parsed =
        url::Url::parse(url_str).map_err(|_| ApiError::BadRequest("invalid URL".into()))?;

    if !allowed_schemes.contains(&parsed.scheme()) {
        return Err(ApiError::BadRequest(format!(
            "URL must use one of these schemes: {allowed_schemes:?}"
        )));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::BadRequest("URL must have a host".into()))?;

    let blocked_hosts = [
        "localhost",
        "169.254.169.254",
        "metadata.google.internal",
        "[::1]",
    ];
    let host_lower = host.to_lowercase();
    if blocked_hosts.iter().any(|b| host_lower == *b) {
        return Err(ApiError::BadRequest(
            "URL must not target internal/metadata endpoints".into(),
        ));
    }

    let bare_ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare_ip.parse::<std::net::IpAddr>()
        && is_private_ip(ip)
    {
        return Err(ApiError::BadRequest(
            "URL must not target private/reserved IP addresses".into(),
        ));
    }

    // DNS rebinding mitigation — resolve hostname and check IPs
    if let Ok(addrs) = (host, 0u16).to_socket_addrs() {
        for addr in addrs {
            if is_private_ip(addr.ip()) {
                return Err(ApiError::BadRequest(
                    "URL hostname resolves to a private/reserved IP address".into(),
                ));
            }
        }
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

/// Convert a git branch name to a K8s-safe DNS label.
///
/// Rules:
/// - Lowercase all characters
/// - Replace `/`, `.`, `_`, `#`, ` ` with `-`
/// - Collapse multiple consecutive `-` into one
/// - Strip leading/trailing `-`
/// - Truncate to 63 characters (K8s DNS label limit)
/// - If empty after processing, return `"preview"`
pub fn slugify_branch(branch: &str) -> String {
    let slug: String = branch
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            '/' | '.' | '_' | '#' | ' ' => '-',
            c if c.is_ascii_alphanumeric() || c == '-' => c,
            _ => '-',
        })
        .collect();

    // Collapse multiple dashes
    let mut result = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash {
                result.push(c);
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    // Strip leading/trailing dashes, truncate
    let trimmed = result.trim_matches('-');
    let truncated = if trimmed.len() > 63 {
        // Truncate at 63, but don't end on a dash
        trimmed[..63].trim_end_matches('-')
    } else {
        trimmed
    };

    if truncated.is_empty() {
        "preview".to_string()
    } else {
        truncated.to_string()
    }
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

    // --- check_labels ---

    #[test]
    fn check_labels_valid() {
        assert!(check_labels(&["bug".into(), "feature".into()]).is_ok());
    }

    #[test]
    fn check_labels_empty() {
        assert!(check_labels(&[]).is_ok());
    }

    #[test]
    fn check_labels_too_many() {
        let labels: Vec<String> = (0..51).map(|i| format!("label{i}")).collect();
        assert!(check_labels(&labels).is_err());
    }

    #[test]
    fn check_labels_one_too_long() {
        assert!(check_labels(&["a".repeat(101)]).is_err());
    }

    #[test]
    fn check_labels_one_empty() {
        assert!(check_labels(&[String::new()]).is_err());
    }

    // --- check_url ---

    #[test]
    fn check_url_valid_https() {
        assert!(check_url("https://example.com/path").is_ok());
    }

    #[test]
    fn check_url_valid_http() {
        assert!(check_url("http://example.com").is_ok());
    }

    #[test]
    fn check_url_rejects_ftp() {
        assert!(check_url("ftp://example.com").is_err());
    }

    #[test]
    fn check_url_rejects_empty() {
        assert!(check_url("").is_err());
    }

    #[test]
    fn check_url_rejects_too_long() {
        let long = format!("https://example.com/{}", "a".repeat(2048));
        assert!(check_url(&long).is_err());
    }

    // --- check_lfs_oid ---

    #[test]
    fn check_lfs_oid_valid() {
        assert!(check_lfs_oid(&"a".repeat(64)).is_ok());
        assert!(check_lfs_oid(&"0123456789abcdef".repeat(4)).is_ok());
    }

    #[test]
    fn check_lfs_oid_too_short() {
        assert!(check_lfs_oid(&"a".repeat(63)).is_err());
    }

    #[test]
    fn check_lfs_oid_too_long() {
        assert!(check_lfs_oid(&"a".repeat(65)).is_err());
    }

    #[test]
    fn check_lfs_oid_non_hex() {
        assert!(check_lfs_oid(&"g".repeat(64)).is_err());
    }

    // --- is_private_ip ---

    #[test]
    fn private_ip_loopback_v4() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_rfc1918_10() {
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_rfc1918_172() {
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_rfc1918_192() {
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_link_local() {
        assert!(is_private_ip("169.254.1.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_public() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn private_ip_loopback_v6() {
        assert!(is_private_ip("::1".parse().unwrap()));
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

    // --- slugify_branch ---

    #[test]
    fn slugify_feature_branch() {
        assert_eq!(slugify_branch("feature/login"), "feature-login");
    }

    #[test]
    fn slugify_dots_underscores_hashes() {
        assert_eq!(slugify_branch("release.1.0_rc#1"), "release-1-0-rc-1");
    }

    #[test]
    fn slugify_collapses_multiple_dashes() {
        assert_eq!(slugify_branch("a//b--c"), "a-b-c");
    }

    #[test]
    fn slugify_truncates_at_63() {
        let long = "a".repeat(100);
        let result = slugify_branch(&long);
        assert!(result.len() <= 63);
        assert_eq!(result, "a".repeat(63));
    }

    #[test]
    fn slugify_truncation_no_trailing_dash() {
        let branch = format!("{}/b", "a".repeat(62));
        let result = slugify_branch(&branch);
        assert!(result.len() <= 63);
        assert!(!result.ends_with('-'));
    }

    #[test]
    fn slugify_empty_returns_preview() {
        assert_eq!(slugify_branch(""), "preview");
    }

    #[test]
    fn slugify_special_chars_only_returns_preview() {
        assert_eq!(slugify_branch("///"), "preview");
    }

    #[test]
    fn slugify_spaces() {
        assert_eq!(slugify_branch("my branch name"), "my-branch-name");
    }

    #[test]
    fn slugify_preserves_existing_dashes() {
        assert_eq!(slugify_branch("already-slugified"), "already-slugified");
    }

    // --- check_name ---

    #[test]
    fn check_name_valid() {
        assert!(check_name("my-service").is_ok());
        assert!(check_name("app_v2.0").is_ok());
        assert!(check_name("a").is_ok());
    }

    #[test]
    fn check_name_empty() {
        assert!(check_name("").is_err());
    }

    #[test]
    fn check_name_too_long() {
        assert!(check_name(&"a".repeat(256)).is_err());
    }

    #[test]
    fn check_name_max_length_ok() {
        assert!(check_name(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn check_name_leading_dot() {
        assert!(check_name(".hidden").is_err());
    }

    #[test]
    fn check_name_trailing_dot() {
        assert!(check_name("name.").is_err());
    }

    #[test]
    fn check_name_invalid_chars() {
        assert!(check_name("name with spaces").is_err());
        assert!(check_name("name/slash").is_err());
        assert!(check_name("name@at").is_err());
    }
}
