use std::net::IpAddr;

use crate::error::ApiError;

pub fn check_length(field: &str, value: &str, min: usize, max: usize) -> Result<(), ApiError> {
    let len = value.len();
    if len < min || len > max {
        return Err(ApiError::BadRequest(format!(
            "{field} must be between {min} and {max} characters (got {len})"
        )));
    }
    Ok(())
}

pub fn check_name(value: &str) -> Result<(), ApiError> {
    check_length("name", value, 1, 255)?;
    if !value
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ApiError::BadRequest(
            "name must contain only alphanumeric characters, hyphens, underscores, or dots".into(),
        ));
    }
    Ok(())
}

pub fn check_email(value: &str) -> Result<(), ApiError> {
    check_length("email", value, 3, 254)?;
    if !value.contains('@') {
        return Err(ApiError::BadRequest("invalid email address".into()));
    }
    Ok(())
}

pub fn check_url(value: &str) -> Result<(), ApiError> {
    check_length("url", value, 1, 2048)?;
    if !value.starts_with("http://") && !value.starts_with("https://") {
        return Err(ApiError::BadRequest(
            "url must use http or https scheme".into(),
        ));
    }
    Ok(())
}

pub fn check_branch_name(value: &str) -> Result<(), ApiError> {
    check_length("branch name", value, 1, 255)?;
    if value.contains("..") || value.contains('\0') {
        return Err(ApiError::BadRequest(
            "branch name must not contain '..' or null bytes".into(),
        ));
    }
    Ok(())
}

pub fn check_labels(labels: &[String]) -> Result<(), ApiError> {
    if labels.len() > 50 {
        return Err(ApiError::BadRequest("max 50 labels".into()));
    }
    for label in labels {
        check_length("label", label, 1, 100)?;
    }
    Ok(())
}

pub fn check_lfs_oid(oid: &str) -> Result<(), ApiError> {
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest(
            "invalid LFS OID: must be 64 hex characters (SHA-256)".into(),
        ));
    }
    Ok(())
}

/// Check whether an IP address is private/reserved (loopback, link-local, etc.).
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()          // 127.0.0.0/8
                || v4.is_private()    // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local() // 169.254/16
                || v4.is_broadcast()  // 255.255.255.255
                || v4.is_unspecified() // 0.0.0.0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()          // ::1
                || v6.is_unspecified() // ::
        }
    }
}

/// Validate a URL against SSRF attacks, accepting only the specified schemes.
/// Blocks private/loopback IPs, link-local, metadata endpoints, and disallowed schemes.
pub fn check_ssrf_url(url_str: &str, allowed_schemes: &[&str]) -> Result<(), ApiError> {
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

    // Block well-known dangerous hostnames
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

    // Block private/reserved IPs
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_private_ip(ip)
    {
        return Err(ApiError::BadRequest(
            "URL must not target private/reserved IP addresses".into(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name() {
        assert!(check_name("foo-bar_123.baz").is_ok());
    }

    #[test]
    fn name_too_long() {
        let long = "a".repeat(256);
        assert!(check_name(&long).is_err());
    }

    #[test]
    fn name_bad_chars() {
        assert!(check_name("foo bar").is_err());
        assert!(check_name("foo/bar").is_err());
    }

    #[test]
    fn valid_email() {
        assert!(check_email("user@example.com").is_ok());
    }

    #[test]
    fn email_missing_at() {
        assert!(check_email("nope").is_err());
    }

    #[test]
    fn valid_lfs_oid() {
        let oid = "a".repeat(64);
        assert!(check_lfs_oid(&oid).is_ok());
    }

    #[test]
    fn invalid_lfs_oid_short() {
        assert!(check_lfs_oid("abc").is_err());
    }

    #[test]
    fn invalid_lfs_oid_nonhex() {
        let oid = "g".repeat(64);
        assert!(check_lfs_oid(&oid).is_err());
    }

    #[test]
    fn branch_name_traversal() {
        assert!(check_branch_name("main").is_ok());
        assert!(check_branch_name("feature/..evil").is_err());
    }

    #[test]
    fn labels_max() {
        let labels: Vec<String> = (0..51).map(|i| format!("label-{i}")).collect();
        assert!(check_labels(&labels).is_err());
        let labels: Vec<String> = (0..50).map(|i| format!("label-{i}")).collect();
        assert!(check_labels(&labels).is_ok());
    }

    #[test]
    fn ssrf_blocks_localhost() {
        assert!(check_ssrf_url("http://localhost:3000/hook", &["http", "https"]).is_err());
    }

    #[test]
    fn ssrf_blocks_metadata() {
        assert!(
            check_ssrf_url(
                "http://169.254.169.254/latest/meta-data/",
                &["http", "https"]
            )
            .is_err()
        );
    }

    #[test]
    fn ssrf_blocks_private_ip() {
        assert!(check_ssrf_url("http://10.0.0.1/hook", &["http", "https"]).is_err());
        assert!(check_ssrf_url("http://192.168.1.1/hook", &["http", "https"]).is_err());
        assert!(check_ssrf_url("http://172.16.0.1/hook", &["http", "https"]).is_err());
        assert!(check_ssrf_url("http://127.0.0.1/hook", &["http", "https"]).is_err());
    }

    #[test]
    fn ssrf_blocks_loopback_v6() {
        assert!(check_ssrf_url("http://[::1]/hook", &["http", "https"]).is_err());
    }

    #[test]
    fn ssrf_blocks_non_http() {
        assert!(check_ssrf_url("ftp://example.com/hook", &["http", "https"]).is_err());
        assert!(check_ssrf_url("file:///etc/passwd", &["http", "https"]).is_err());
    }

    #[test]
    fn ssrf_allows_public_url() {
        assert!(check_ssrf_url("https://example.com/webhook", &["http", "https"]).is_ok());
        assert!(check_ssrf_url("http://hooks.slack.com/services/xxx", &["http", "https"]).is_ok());
    }

    #[test]
    fn private_ip_detection() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.0.1".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("169.254.1.1".parse().unwrap()));
        assert!(is_private_ip("::1".parse().unwrap()));
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
    }
}
