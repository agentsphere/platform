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
}
