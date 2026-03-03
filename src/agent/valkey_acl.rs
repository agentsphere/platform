use fred::interfaces::ClientLike;
use uuid::Uuid;

use super::error::AgentError;

/// Credentials for a per-session Valkey ACL user.
/// Custom `Debug` impl redacts `password` and `url` to prevent accidental logging.
pub struct SessionValkeyCredentials {
    pub username: String,
    #[allow(dead_code)] // Stored for completeness; password is embedded in `url`
    pub password: String,
    /// Full Redis URL for the agent pod: `redis://{username}:{password}@{host}`
    pub url: String,
}

impl std::fmt::Debug for SessionValkeyCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionValkeyCredentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("url", &"[REDACTED]")
            .finish()
    }
}

/// Channel name for session events (agent → platform).
pub fn events_channel(session_id: Uuid) -> String {
    format!("session:{session_id}:events")
}

/// Channel name for session input (platform → agent).
pub fn input_channel(session_id: Uuid) -> String {
    format!("session:{session_id}:input")
}

/// ACL username for a session.
fn acl_username(session_id: Uuid) -> String {
    format!("session-{session_id}")
}

/// Generate a cryptographically random password (32 bytes, hex-encoded = 64 chars).
fn generate_password() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

/// Build the arguments for `ACL SETUSER`.
fn build_acl_setuser_args(username: &str, password: &str, session_id: Uuid) -> Vec<String> {
    vec![
        "SETUSER".to_owned(),
        username.to_owned(),
        "on".to_owned(),
        format!(">{password}"),
        "resetkeys".to_owned(),
        "resetchannels".to_owned(),
        "-@all".to_owned(),
        format!("&session:{session_id}:*"),
        "+subscribe".to_owned(),
        "+publish".to_owned(),
        "+unsubscribe".to_owned(),
        "+ping".to_owned(),
    ]
}

/// Build the arguments for `ACL DELUSER`.
fn build_acl_deluser_args(username: &str) -> Vec<String> {
    vec!["DELUSER".to_owned(), username.to_owned()]
}

/// Build the full Valkey URL with credentials.
fn build_valkey_url(username: &str, password: &str, host: &str) -> String {
    format!("redis://{username}:{password}@{host}")
}

/// Create a scoped Valkey ACL user for an agent session.
///
/// ACL rule: `resetkeys resetchannels -@all &session:{id}:* +subscribe +publish +unsubscribe +ping`
///
/// The baseline `resetkeys resetchannels -@all` ensures zero default access.
/// `+ping` is required for fred keepalive health checks.
/// Uses explicit commands (not `+@pubsub`) to exclude `PUBSUB CHANNELS` diagnostic.
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn create_session_acl(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    valkey_agent_host: &str,
) -> Result<SessionValkeyCredentials, AgentError> {
    let username = acl_username(session_id);
    let password = generate_password();
    let args = build_acl_setuser_args(&username, &password, session_id);

    let _result: String = valkey
        .custom(
            fred::types::CustomCommand::new_static("ACL", None, false),
            args,
        )
        .await
        .map_err(|e| AgentError::Other(anyhow::anyhow!("ACL SETUSER failed: {e}")))?;

    let url = build_valkey_url(&username, &password, valkey_agent_host);

    Ok(SessionValkeyCredentials {
        username,
        password,
        url,
    })
}

/// Delete a per-session Valkey ACL user. Idempotent — succeeds even if user doesn't exist.
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn delete_session_acl(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> Result<(), AgentError> {
    let username = acl_username(session_id);
    let args = build_acl_deluser_args(&username);

    // ACL DELUSER returns the number of deleted users (0 or 1).
    // We ignore the count — idempotent deletion.
    let _count: i64 = valkey
        .custom(
            fred::types::CustomCommand::new_static("ACL", None, false),
            args,
        )
        .await
        .map_err(|e| AgentError::Other(anyhow::anyhow!("ACL DELUSER failed: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acl_username_format() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            acl_username(id),
            "session-550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_generate_acl_password_length() {
        let pw = generate_password();
        assert_eq!(pw.len(), 64, "password should be 64 hex chars (32 bytes)");
    }

    #[test]
    fn test_generate_acl_password_unique() {
        let pw1 = generate_password();
        let pw2 = generate_password();
        assert_ne!(pw1, pw2, "two passwords should differ");
    }

    #[test]
    fn test_generate_acl_password_hex_only() {
        let pw = generate_password();
        assert!(
            pw.chars().all(|c| c.is_ascii_hexdigit()),
            "password should be hex only, got: {pw}"
        );
    }

    #[test]
    fn test_build_acl_setuser_commands() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let args =
            build_acl_setuser_args("session-550e8400-e29b-41d4-a716-446655440000", "abc123", id);
        assert_eq!(args[0], "SETUSER");
        assert_eq!(args[1], "session-550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(args[2], "on");
        assert_eq!(args[3], ">abc123");
        assert_eq!(args[4], "resetkeys");
        assert_eq!(args[5], "resetchannels");
        assert_eq!(args[6], "-@all");
        assert_eq!(args[7], "&session:550e8400-e29b-41d4-a716-446655440000:*");
        // Verify specific commands present
        assert!(args.contains(&"+subscribe".to_owned()));
        assert!(args.contains(&"+publish".to_owned()));
        assert!(args.contains(&"+unsubscribe".to_owned()));
        assert!(args.contains(&"+ping".to_owned()));
    }

    #[test]
    fn test_build_acl_setuser_no_psubscribe() {
        let id = Uuid::new_v4();
        let args = build_acl_setuser_args("test", "pw", id);
        assert!(!args.contains(&"+psubscribe".to_owned()));
        assert!(!args.contains(&"+@pubsub".to_owned()));
    }

    #[test]
    fn test_build_acl_setuser_includes_ping() {
        let id = Uuid::new_v4();
        let args = build_acl_setuser_args("test", "pw", id);
        assert!(args.contains(&"+ping".to_owned()));
    }

    #[test]
    fn test_channel_pattern_events() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            events_channel(id),
            "session:550e8400-e29b-41d4-a716-446655440000:events"
        );
    }

    #[test]
    fn test_channel_pattern_input() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            input_channel(id),
            "session:550e8400-e29b-41d4-a716-446655440000:input"
        );
    }

    #[test]
    fn test_build_acl_deluser_command() {
        let args = build_acl_deluser_args("session-550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            args,
            vec!["DELUSER", "session-550e8400-e29b-41d4-a716-446655440000"]
        );
    }

    #[test]
    fn test_build_valkey_url_with_credentials() {
        let url = build_valkey_url("session-abc", "secret123", "myhost:6379");
        assert_eq!(url, "redis://session-abc:secret123@myhost:6379");
    }

    #[test]
    fn test_build_valkey_url_preserves_host_port() {
        let url = build_valkey_url("user", "pw", "valkey.platform.svc.cluster.local:6379");
        assert!(url.contains("valkey.platform.svc.cluster.local:6379"));
    }

    #[test]
    fn test_credentials_debug_redacts_password() {
        let creds = SessionValkeyCredentials {
            username: "session-abc".into(),
            password: "supersecret".into(),
            url: "redis://session-abc:supersecret@host:6379".into(),
        };
        let debug = format!("{creds:?}");
        assert!(debug.contains("session-abc"), "username should be visible");
        assert!(!debug.contains("supersecret"), "password must be redacted");
        assert!(debug.contains("[REDACTED]"), "should show [REDACTED]");
    }

    #[test]
    fn test_channel_names_are_session_scoped() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        assert_ne!(events_channel(id1), events_channel(id2));
        assert_ne!(input_channel(id1), input_channel(id2));
    }

    #[test]
    fn test_acl_setuser_args_count() {
        let id = Uuid::new_v4();
        let args = build_acl_setuser_args("test", "pw", id);
        // SETUSER, username, on, >pw, resetkeys, resetchannels, -@all,
        // &session:{id}:*, +subscribe, +publish, +unsubscribe, +ping = 12
        assert_eq!(args.len(), 12);
    }

    #[test]
    fn test_password_not_zero() {
        let pw = generate_password();
        // Should not be all zeros (astronomically unlikely with 256 bits of entropy)
        assert_ne!(pw, "0".repeat(64));
    }
}
