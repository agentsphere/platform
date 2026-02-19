use std::sync::LazyLock;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

/// Pre-computed argon2 hash used for timing-safe login when the user doesn't exist.
/// This ensures that login attempts for non-existent users take the same time as
/// verifying a real password, preventing user enumeration via timing.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    hash_password("__dummy_password_for_timing_safety__")
        .expect("dummy hash generation must succeed")
});

pub fn dummy_hash() -> &'static str {
    &DUMMY_HASH
}

pub fn hash_password(plain: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("password hash failed: {e}"))?
        .to_string();
    Ok(hash)
}

pub fn verify_password(plain: &str, hash: &str) -> anyhow::Result<bool> {
    let parsed =
        PasswordHash::new(hash).map_err(|e| anyhow::anyhow!("invalid password hash: {e}"))?;
    Ok(Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let plain = "correcthorsebatterystaple";
        let hash = hash_password(plain).unwrap();

        assert!(hash.starts_with("$argon2"));
        assert!(verify_password(plain, &hash).unwrap());
    }

    #[test]
    fn wrong_password_fails() {
        let hash = hash_password("secret123").unwrap();
        assert!(!verify_password("wrong", &hash).unwrap());
    }

    #[test]
    fn different_hashes_for_same_password() {
        let h1 = hash_password("same").unwrap();
        let h2 = hash_password("same").unwrap();
        assert_ne!(h1, h2); // different salts
    }
}
