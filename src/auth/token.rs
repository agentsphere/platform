use sha2::{Digest, Sha256};

/// Generate a session token. Returns `(raw_token, sha256_hash)`.
/// Format: `plat_` + 32 random bytes as hex (69 chars total).
pub fn generate_session_token() -> (String, String) {
    let raw = generate_raw("plat_");
    let hash = hash_token(&raw);
    (raw, hash)
}

/// Generate an API token. Returns `(raw_token, sha256_hash)`.
/// Format: `plat_api_` + 32 random bytes as hex (72 chars total).
pub fn generate_api_token() -> (String, String) {
    let raw = generate_raw("plat_api_");
    let hash = hash_token(&raw);
    (raw, hash)
}

/// SHA-256 hash of a token string, returned as lowercase hex.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_raw(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    format!("{prefix}{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_token_format() {
        let (raw, hash) = generate_session_token();
        assert!(raw.starts_with("plat_"));
        assert_eq!(raw.len(), 5 + 64); // "plat_" + 32 bytes hex
        assert_eq!(hash.len(), 64); // sha256 hex
    }

    #[test]
    fn api_token_format() {
        let (raw, hash) = generate_api_token();
        assert!(raw.starts_with("plat_api_"));
        assert_eq!(raw.len(), 9 + 64); // "plat_api_" + 32 bytes hex
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn hash_is_deterministic() {
        let h1 = hash_token("plat_abc123");
        let h2 = hash_token("plat_abc123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_tokens_different_hashes() {
        let (raw1, hash1) = generate_session_token();
        let (raw2, hash2) = generate_session_token();
        assert_ne!(raw1, raw2);
        assert_ne!(hash1, hash2);
    }
}
