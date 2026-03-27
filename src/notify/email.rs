use crate::config::Config;

/// Send a plain-text email via SMTP.
///
/// Returns early (with a warning log) if SMTP is not configured.
/// Sanitizes `to` and `subject` to prevent header injection.
#[tracing::instrument(skip(config, body), fields(%to), err)]
pub async fn send(config: &Config, to: &str, subject: &str, body: &str) -> anyhow::Result<()> {
    use lettre::message::Mailbox;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let Some(ref smtp_host) = config.smtp_host else {
        tracing::warn!("SMTP not configured — email not sent");
        return Ok(());
    };

    // Email header injection prevention: reject newlines in to/subject
    if to.contains('\n') || to.contains('\r') {
        anyhow::bail!("email 'to' address contains invalid characters");
    }
    if subject.contains('\n') || subject.contains('\r') {
        anyhow::bail!("email subject contains invalid characters");
    }

    let from: Mailbox = config
        .smtp_from
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid smtp_from address '{}': {e}", config.smtp_from))?;

    let to_mailbox: Mailbox = to
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid recipient address '{to}': {e}"))?;

    let message = Message::builder()
        .from(from)
        .to(to_mailbox)
        .subject(subject)
        .body(body.to_owned())
        .map_err(|e| anyhow::anyhow!("failed to build email: {e}"))?;

    let mut transport = if config.smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)
            .map_err(|e| anyhow::anyhow!("SMTP relay setup failed: {e}"))?
            .port(465)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| anyhow::anyhow!("SMTP relay setup failed: {e}"))?
            .port(config.smtp_port)
    };

    if let Some(ref username) = config.smtp_username {
        let password = config.smtp_password.as_deref().unwrap_or("");
        transport = transport.credentials(Credentials::new(username.clone(), password.to_owned()));
    }

    let transport = transport.build();

    // One retry on transient failure
    match transport.send(message.clone()).await {
        Ok(_) => {
            tracing::info!(to, subject, "email sent");
            Ok(())
        }
        Err(first_err) => {
            tracing::warn!(error = %first_err, "email send failed, retrying once");
            transport
                .send(message)
                .await
                .map_err(|e| anyhow::anyhow!("email send failed after retry: {e}"))?;
            tracing::info!(to, subject, "email sent on retry");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::test_default()
    }

    #[tokio::test]
    async fn send_without_smtp_host_is_noop() {
        let config = test_config();
        // Should return Ok without sending anything
        let result = send(&config, "user@example.com", "test", "body").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn reject_newline_in_to() {
        let mut config = test_config();
        config.smtp_host = Some("localhost".into());
        let result = send(
            &config,
            "user@example.com\nBcc: evil@attacker.com",
            "test",
            "body",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reject_newline_in_subject() {
        let mut config = test_config();
        config.smtp_host = Some("localhost".into());
        let result = send(
            &config,
            "user@example.com",
            "test\r\nBcc: evil@attacker.com",
            "body",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reject_cr_in_to() {
        let mut config = test_config();
        config.smtp_host = Some("localhost".into());
        let result = send(
            &config,
            "user@example.com\rBcc: evil@attacker.com",
            "test",
            "body",
        )
        .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid characters"),
            "should mention invalid characters"
        );
    }

    #[tokio::test]
    async fn reject_lf_only_in_subject() {
        let mut config = test_config();
        config.smtp_host = Some("localhost".into());
        let result = send(
            &config,
            "user@example.com",
            "test\nBcc: evil@attacker.com",
            "body",
        )
        .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid characters"),
            "should mention invalid characters"
        );
    }

    #[tokio::test]
    async fn invalid_from_address_returns_error() {
        let mut config = test_config();
        config.smtp_host = Some("localhost".into());
        config.smtp_from = "not-an-email".into();
        let result = send(&config, "user@example.com", "test", "body").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid smtp_from"),
            "should report invalid smtp_from"
        );
    }

    #[tokio::test]
    async fn invalid_to_address_returns_error() {
        let mut config = test_config();
        config.smtp_host = Some("localhost".into());
        let result = send(&config, "not-an-email", "test", "body").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid recipient"),
            "should report invalid recipient address"
        );
    }

    #[tokio::test]
    async fn send_without_smtp_host_is_noop_regardless_of_inputs() {
        // Even with invalid addresses, no-op when SMTP is not configured
        let config = test_config();
        let result = send(&config, "not-valid", "subject", "body").await;
        assert!(result.is_ok(), "should be noop when SMTP is not configured");
    }
}
