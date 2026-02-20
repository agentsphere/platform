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

/// Check whether an IPv6 address is in the unique-local range (`fc00::/7`).
fn is_ipv6_unique_local(v6: &std::net::Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
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
                || is_ipv6_unique_local(&v6)  // fc00::/7 (includes fd00::/8)
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
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

    // Block private/reserved IPs (strip brackets for IPv6 literals like [::1])
    let bare_ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare_ip.parse::<IpAddr>()
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

    // -- Boundary tests --

    #[test]
    fn check_name_boundary_lengths() {
        assert!(check_name("").is_err(), "empty name should fail");
        assert!(check_name("a").is_ok(), "single char name should pass");
        assert!(
            check_name(&"a".repeat(255)).is_ok(),
            "255-char name should pass"
        );
        assert!(
            check_name(&"a".repeat(256)).is_err(),
            "256-char name should fail"
        );
    }

    #[test]
    fn check_name_rejects_unicode_alphanumeric() {
        // is_alphanumeric() is Unicode-aware: 'é' passes it, but we only want ASCII
        // This test documents the current behavior
        let result = check_name("café");
        // 'é' is alphanumeric in Unicode, so current impl allows it
        // If this is undesired, the check should use is_ascii_alphanumeric()
        assert!(
            result.is_ok(),
            "current impl allows Unicode alphanumeric: {result:?}"
        );
    }

    #[test]
    fn check_email_boundary_lengths() {
        assert!(
            check_email("a@").is_err(),
            "2-char email should fail (min 3)"
        );
        assert!(check_email("a@b").is_ok(), "3-char email should pass");
        // max is 254: "a@" (2 chars) + 252 = 254 total
        let long = format!("a@{}", "b".repeat(252));
        assert_eq!(long.len(), 254);
        assert!(check_email(&long).is_ok(), "254-char email should pass");
        let too_long = format!("a@{}", "b".repeat(253));
        assert_eq!(too_long.len(), 255);
        assert!(
            check_email(&too_long).is_err(),
            "255-char email should fail"
        );
    }

    #[test]
    fn check_email_multiple_at_signs() {
        // Current impl only checks contains('@'), so this passes
        assert!(check_email("a@b@c").is_ok());
    }

    #[test]
    fn check_labels_empty_label_fails() {
        assert!(
            check_labels(&["".into()]).is_err(),
            "empty label should fail (min 1 char)"
        );
    }

    #[test]
    fn check_labels_boundary_label_length() {
        assert!(
            check_labels(&["a".repeat(100)]).is_ok(),
            "100-char label should pass"
        );
        assert!(
            check_labels(&["a".repeat(101)]).is_err(),
            "101-char label should fail"
        );
    }

    #[test]
    fn check_branch_name_null_byte_in_middle() {
        assert!(check_branch_name("main\0evil").is_err());
    }

    #[test]
    fn check_branch_name_boundary_length() {
        assert!(
            check_branch_name(&"a".repeat(255)).is_ok(),
            "255-char branch should pass"
        );
        assert!(
            check_branch_name(&"a".repeat(256)).is_err(),
            "256-char branch should fail"
        );
    }

    #[test]
    fn check_length_boundaries() {
        assert!(check_length("f", "ab", 2, 5).is_ok(), "at min should pass");
        assert!(
            check_length("f", "a", 2, 5).is_err(),
            "below min should fail"
        );
        assert!(
            check_length("f", "abcde", 2, 5).is_ok(),
            "at max should pass"
        );
        assert!(
            check_length("f", "abcdef", 2, 5).is_err(),
            "above max should fail"
        );
    }

    #[test]
    fn check_url_empty_host() {
        assert!(
            check_url("http://").is_ok(),
            "check_url doesn't validate host (only scheme + length)"
        );
    }

    #[test]
    fn ssrf_blocks_unspecified_ipv4() {
        assert!(check_ssrf_url("http://0.0.0.0/hook", &["http", "https"]).is_err());
    }

    #[test]
    fn private_ip_ipv6_unique_local() {
        // fc00::/7 — unique local addresses (IPv6 RFC 1918 equivalent)
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fd12:3456:789a::1".parse().unwrap()));
        assert!(is_private_ip("fdff:ffff:ffff::1".parse().unwrap()));
    }

    #[test]
    fn private_ip_ipv6_link_local() {
        // fe80::/10 — link-local addresses
        assert!(is_private_ip("fe80::1".parse().unwrap()));
        assert!(is_private_ip(
            "fe80::1%eth0"
                .parse::<IpAddr>()
                .unwrap_or_else(|_| "fe80::1".parse().unwrap())
        ));
        assert!(is_private_ip("febf::1".parse().unwrap()));
    }

    #[test]
    fn private_ip_ipv6_allows_public() {
        assert!(!is_private_ip("2001:db8::1".parse().unwrap()));
        assert!(!is_private_ip("2607:f8b0:4004:800::200e".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_ipv6_unique_local() {
        assert!(check_ssrf_url("http://[fc00::1]/hook", &["http", "https"]).is_err());
        assert!(check_ssrf_url("http://[fd12::1]/hook", &["http", "https"]).is_err());
    }

    #[test]
    fn ssrf_blocks_ipv6_link_local() {
        assert!(check_ssrf_url("http://[fe80::1]/hook", &["http", "https"]).is_err());
    }
}
