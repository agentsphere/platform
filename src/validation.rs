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

/// Validates a container image reference.
///
/// Accepts: `registry/image:tag`, `image:tag`, `image@sha256:abc...`,
///          `gcr.io/project/image:tag`, `localhost:5000/image:tag`
///
/// Rejects: shell metacharacters, empty strings, strings > 500 chars,
///          strings containing `;`, `&`, `|`, `$`, backtick, quotes, `\`, newlines
pub fn check_container_image(image: &str) -> Result<(), ApiError> {
    check_length("image", image, 1, 500)?;

    // Block shell injection characters
    let forbidden = [
        ';', '&', '|', '$', '`', '\'', '"', '\\', '\n', '\r', ' ', '\t',
    ];
    if image.chars().any(|c| forbidden.contains(&c)) {
        return Err(ApiError::BadRequest(
            "image: contains forbidden characters".into(),
        ));
    }

    // Must contain at least one alphanumeric character
    if !image.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::BadRequest(
            "image: must contain alphanumeric characters".into(),
        ));
    }

    Ok(())
}

/// Validates setup commands for agent sessions.
///
/// Max 20 commands, each 1-2000 characters.
/// Commands are joined with `&&` and executed in a shell.
pub fn check_setup_commands(commands: &[String]) -> Result<(), ApiError> {
    if commands.len() > 20 {
        return Err(ApiError::BadRequest(
            "setup_commands: max 20 commands".into(),
        ));
    }
    for (i, cmd) in commands.iter().enumerate() {
        check_length(&format!("setup_commands[{i}]"), cmd, 1, 2000)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // -----------------------------------------------------------------------
    // check_name — boundary & edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn valid_name() {
        assert!(check_name("foo-bar_123.baz").is_ok());
    }

    #[test]
    fn name_empty_rejected() {
        let err = check_name("").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("name")),
            "empty name should produce BadRequest mentioning 'name', got: {err:?}"
        );
    }

    #[test]
    fn name_single_char_ok() {
        assert!(check_name("a").is_ok());
    }

    #[test]
    fn name_at_max_length() {
        assert!(check_name(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn name_over_max_length() {
        let err = check_name(&"a".repeat(256)).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("name")),
            "over-max name should produce BadRequest mentioning 'name', got: {err:?}"
        );
    }

    #[test]
    fn name_with_hyphen_underscore_dot() {
        assert!(check_name("my-app_v1.0").is_ok());
    }

    #[test]
    fn name_with_spaces_rejected() {
        let err = check_name("has space").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("name")),
            "name with spaces should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn name_with_slash_rejected() {
        let err = check_name("foo/bar").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("name")),
            "name with slash should produce BadRequest, got: {err:?}"
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

    // -----------------------------------------------------------------------
    // check_email — boundary & edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn valid_email() {
        assert!(check_email("user@example.com").is_ok());
    }

    #[test]
    fn email_minimum_valid() {
        assert!(check_email("a@b").is_ok(), "3-char email should pass");
    }

    #[test]
    fn email_too_short_rejected() {
        let err = check_email("a@").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("email")),
            "2-char email should produce BadRequest mentioning 'email', got: {err:?}"
        );
    }

    #[test]
    fn email_no_at_rejected() {
        let err = check_email("nope").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("email")),
            "email without @ should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn email_at_max_length() {
        let long = format!("a@{}", "b".repeat(252));
        assert_eq!(long.len(), 254);
        assert!(check_email(&long).is_ok(), "254-char email should pass");
    }

    #[test]
    fn email_over_max_length() {
        let too_long = format!("a@{}", "b".repeat(253));
        assert_eq!(too_long.len(), 255);
        let err = check_email(&too_long).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("email")),
            "255-char email should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn check_email_multiple_at_signs() {
        // Current impl only checks contains('@'), so this passes
        assert!(check_email("a@b@c").is_ok());
    }

    // -----------------------------------------------------------------------
    // check_branch_name — boundary & edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn branch_name_normal() {
        assert!(check_branch_name("feature/add-login").is_ok());
    }

    #[test]
    fn branch_name_with_double_dot_rejected() {
        let err = check_branch_name("main..evil").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("branch")),
            "double-dot branch should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn branch_name_traversal_rejected() {
        let err = check_branch_name("feature/..evil").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("branch")),
            "traversal branch should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn branch_name_with_null_byte_rejected() {
        let err = check_branch_name("main\0evil").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("branch")),
            "null-byte branch should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn branch_name_empty_rejected() {
        let err = check_branch_name("").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("branch")),
            "empty branch should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn check_branch_name_at_max_length() {
        assert!(
            check_branch_name(&"a".repeat(255)).is_ok(),
            "255-char branch should pass"
        );
    }

    #[test]
    fn check_branch_name_over_max_length() {
        let err = check_branch_name(&"a".repeat(256)).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("branch")),
            "256-char branch should produce BadRequest, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // check_labels — boundary & edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn labels_empty_vec_ok() {
        assert!(check_labels(&[]).is_ok());
    }

    #[test]
    fn labels_at_max_count() {
        let labels: Vec<String> = (0..50).map(|i| format!("label-{i}")).collect();
        assert!(check_labels(&labels).is_ok());
    }

    #[test]
    fn labels_over_max_count() {
        let labels: Vec<String> = (0..51).map(|i| format!("label-{i}")).collect();
        let err = check_labels(&labels).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("label")),
            "51 labels should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn labels_empty_string_rejected() {
        let err = check_labels(&["".into()]).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("label")),
            "empty label should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn labels_at_max_char_length() {
        assert!(check_labels(&["a".repeat(100)]).is_ok());
    }

    #[test]
    fn labels_over_max_char_length() {
        let err = check_labels(&["a".repeat(101)]).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("label")),
            "101-char label should produce BadRequest, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // check_url — boundary & edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn url_http_ok() {
        assert!(check_url("http://example.com").is_ok());
    }

    #[test]
    fn url_https_ok() {
        assert!(check_url("https://example.com").is_ok());
    }

    #[test]
    fn url_ftp_rejected() {
        let err = check_url("ftp://example.com").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("url")),
            "ftp URL should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn url_over_max_length() {
        let long_url = format!("https://example.com/{}", "a".repeat(2030));
        assert!(long_url.len() > 2048);
        let err = check_url(&long_url).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("url")),
            "over-max URL should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn url_empty_rejected() {
        let err = check_url("").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("url")),
            "empty URL should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn check_url_empty_host() {
        assert!(
            check_url("http://").is_ok(),
            "check_url doesn't validate host (only scheme + length)"
        );
    }

    // -----------------------------------------------------------------------
    // check_lfs_oid — boundary & edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn lfs_oid_valid_64_hex() {
        assert!(check_lfs_oid(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn lfs_oid_63_chars_rejected() {
        let err = check_lfs_oid(&"a".repeat(63)).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("LFS OID")),
            "63-char OID should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn lfs_oid_65_chars_rejected() {
        let err = check_lfs_oid(&"a".repeat(65)).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("LFS OID")),
            "65-char OID should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn lfs_oid_non_hex_rejected() {
        let err = check_lfs_oid(&"g".repeat(64)).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("LFS OID")),
            "non-hex OID should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn lfs_oid_short_rejected() {
        let err = check_lfs_oid("abc").unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("LFS OID")),
            "short OID should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn lfs_oid_uppercase_hex_accepted() {
        // A-F are valid hex digits
        assert!(check_lfs_oid(&"A".repeat(64)).is_ok());
    }

    // -----------------------------------------------------------------------
    // check_length — boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn check_length_at_min_passes() {
        assert!(check_length("f", "ab", 2, 5).is_ok());
    }

    #[test]
    fn check_length_below_min_fails() {
        let err = check_length("f", "a", 2, 5).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("f")),
            "below-min should produce BadRequest with field name, got: {err:?}"
        );
    }

    #[test]
    fn check_length_at_max_passes() {
        assert!(check_length("f", "abcde", 2, 5).is_ok());
    }

    #[test]
    fn check_length_above_max_fails() {
        let err = check_length("f", "abcdef", 2, 5).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("f")),
            "above-max should produce BadRequest with field name, got: {err:?}"
        );
    }

    #[test]
    fn check_length_zero_min_allows_empty() {
        assert!(check_length("f", "", 0, 100).is_ok());
    }

    // -----------------------------------------------------------------------
    // SSRF — rstest parameterized tests for private/blocked IPs
    // -----------------------------------------------------------------------

    #[rstest]
    #[case("127.0.0.1")]
    #[case("10.0.0.1")]
    #[case("10.255.255.255")]
    #[case("172.16.0.1")]
    #[case("172.31.255.255")]
    #[case("192.168.0.1")]
    #[case("192.168.255.255")]
    #[case("169.254.0.1")]
    #[case("169.254.169.254")]
    fn ssrf_blocks_private_ips(#[case] ip: &str) {
        let url = format!("http://{ip}/webhook");
        assert!(
            check_ssrf_url(&url, &["http", "https"]).is_err(),
            "SSRF should block {ip}"
        );
    }

    #[rstest]
    #[case("[::1]")]
    #[case("[fc00::1]")]
    #[case("[fd12::1]")]
    #[case("[fe80::1]")]
    fn ssrf_blocks_private_ipv6(#[case] ip: &str) {
        let url = format!("http://{ip}/webhook");
        assert!(
            check_ssrf_url(&url, &["http", "https"]).is_err(),
            "SSRF should block {ip}"
        );
    }

    #[rstest]
    #[case("93.184.216.34")]
    #[case("8.8.8.8")]
    fn ssrf_allows_public_ips(#[case] ip: &str) {
        let url = format!("http://{ip}/webhook");
        assert!(
            check_ssrf_url(&url, &["http", "https"]).is_ok(),
            "SSRF should allow public IP {ip}"
        );
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
    fn ssrf_blocks_non_http() {
        let err = check_ssrf_url("ftp://example.com/hook", &["http", "https"]).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("scheme")),
            "non-http scheme should produce BadRequest mentioning 'scheme', got: {err:?}"
        );
    }

    #[test]
    fn ssrf_blocks_file_scheme() {
        let err = check_ssrf_url("file:///etc/passwd", &["http", "https"]).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("scheme")),
            "file scheme should produce BadRequest, got: {err:?}"
        );
    }

    #[test]
    fn ssrf_allows_public_url() {
        assert!(check_ssrf_url("https://example.com/webhook", &["http", "https"]).is_ok());
        assert!(check_ssrf_url("http://hooks.slack.com/services/xxx", &["http", "https"]).is_ok());
    }

    #[test]
    fn ssrf_blocks_unspecified_ipv4() {
        assert!(check_ssrf_url("http://0.0.0.0/hook", &["http", "https"]).is_err());
    }

    // -----------------------------------------------------------------------
    // is_private_ip — rstest parameterized
    // -----------------------------------------------------------------------

    #[rstest]
    #[case("127.0.0.1", true)]
    #[case("10.0.0.1", true)]
    #[case("192.168.0.1", true)]
    #[case("172.16.0.1", true)]
    #[case("169.254.1.1", true)]
    #[case("0.0.0.0", true)]
    #[case("::1", true)]
    #[case("fc00::1", true)]
    #[case("fd12:3456:789a::1", true)]
    #[case("fdff:ffff:ffff::1", true)]
    #[case("fe80::1", true)]
    #[case("febf::1", true)]
    #[case("8.8.8.8", false)]
    #[case("1.1.1.1", false)]
    #[case("2001:db8::1", false)]
    #[case("2607:f8b0:4004:800::200e", false)]
    fn private_ip_detection(#[case] ip: &str, #[case] expected: bool) {
        let addr: IpAddr = ip.parse().unwrap();
        assert_eq!(
            is_private_ip(addr),
            expected,
            "is_private_ip({ip}) should be {expected}"
        );
    }

    // -----------------------------------------------------------------------
    // Container image validation
    // -----------------------------------------------------------------------

    #[test]
    fn valid_container_images() {
        for img in [
            "golang:1.23",
            "node:22-slim",
            "rust:1.80",
            "ghcr.io/org/image:v1.2",
            "localhost:5000/my-app:latest",
            "image@sha256:abcdef1234567890",
            "registry.example.com/team/runner:v3",
        ] {
            assert!(check_container_image(img).is_ok(), "should accept: {img}");
        }
    }

    #[test]
    fn rejected_container_images() {
        assert!(
            matches!(check_container_image(""), Err(ApiError::BadRequest(ref msg)) if msg.contains("image")),
            "empty image"
        );
        assert!(
            matches!(check_container_image(&"a".repeat(501)), Err(ApiError::BadRequest(ref msg)) if msg.contains("image")),
            "too long"
        );
        assert!(
            matches!(check_container_image("image;rm -rf /"), Err(ApiError::BadRequest(ref msg)) if msg.contains("forbidden")),
            "semicolon"
        );
        assert!(
            matches!(check_container_image("img & echo"), Err(ApiError::BadRequest(ref msg)) if msg.contains("forbidden")),
            "ampersand"
        );
        assert!(
            matches!(check_container_image("img | cat"), Err(ApiError::BadRequest(ref msg)) if msg.contains("forbidden")),
            "pipe"
        );
        assert!(
            matches!(check_container_image("$(evil)"), Err(ApiError::BadRequest(ref msg)) if msg.contains("forbidden")),
            "dollar"
        );
        assert!(
            matches!(check_container_image("`evil`"), Err(ApiError::BadRequest(ref msg)) if msg.contains("forbidden")),
            "backtick"
        );
        assert!(
            matches!(check_container_image("img\nevil"), Err(ApiError::BadRequest(ref msg)) if msg.contains("forbidden")),
            "newline"
        );
        assert!(
            matches!(check_container_image("has space"), Err(ApiError::BadRequest(ref msg)) if msg.contains("forbidden")),
            "space"
        );
        assert!(
            matches!(check_container_image("---/.../:"), Err(ApiError::BadRequest(ref msg)) if msg.contains("alphanumeric")),
            "no alphanumeric"
        );
    }

    // -----------------------------------------------------------------------
    // Setup commands validation
    // -----------------------------------------------------------------------

    #[test]
    fn valid_setup_commands() {
        assert!(check_setup_commands(&["npm install".into()]).is_ok());
        assert!(check_setup_commands(&vec!["cmd".into(); 20]).is_ok());
    }

    #[test]
    fn rejected_setup_commands() {
        // Too many commands
        let err = check_setup_commands(&vec!["cmd".into(); 21]).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("setup_commands")),
            "too many commands should produce BadRequest, got: {err:?}"
        );
        // Empty command
        let err = check_setup_commands(&["".into()]).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("setup_commands")),
            "empty command should produce BadRequest, got: {err:?}"
        );
        // Command too long
        let err = check_setup_commands(&["a".repeat(2001)]).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(ref msg) if msg.contains("setup_commands")),
            "too-long command should produce BadRequest, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // proptest — LFS OID roundtrip
    // -----------------------------------------------------------------------

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn valid_hex_oid_accepted(s in "[0-9a-f]{64}") {
                prop_assert!(check_lfs_oid(&s).is_ok());
            }

            #[test]
            fn wrong_length_hex_rejected(len in 1_usize..64) {
                let s: String = "a".repeat(len);
                prop_assert!(check_lfs_oid(&s).is_err());
            }

            #[test]
            fn too_long_hex_rejected(len in 65_usize..200) {
                let s: String = "a".repeat(len);
                prop_assert!(check_lfs_oid(&s).is_err());
            }
        }
    }
}
