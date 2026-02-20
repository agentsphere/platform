use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::rate_limit;
use crate::error::ApiError;
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyChannel {
    InApp,
    Email,
    Webhook,
}

impl NotifyChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InApp => "in_app",
            Self::Email => "email",
            Self::Webhook => "webhook",
        }
    }
}

pub struct NewNotification {
    pub user_id: Uuid,
    pub notification_type: String,
    pub subject: String,
    pub body: Option<String>,
    pub channel: NotifyChannel,
    pub ref_type: Option<String>,
    pub ref_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Central notification dispatch. Inserts the notification into the DB and
/// routes it through the appropriate channel.
#[tracing::instrument(
    skip(state, notification),
    fields(
        user_id = %notification.user_id,
        notification_type = %notification.notification_type,
        channel = notification.channel.as_str()
    ),
    err
)]
pub async fn notify(state: &AppState, notification: NewNotification) -> Result<(), ApiError> {
    // Rate limit: max 100 notifications per user per hour
    let user_key = notification.user_id.to_string();
    rate_limit::check_rate(&state.valkey, "notify", &user_key, 100, 3600).await?;

    // Insert notification row
    let row = sqlx::query!(
        r#"
        INSERT INTO notifications (user_id, notification_type, subject, body, channel, status, ref_type, ref_id)
        VALUES ($1, $2, $3, $4, $5, 'pending', $6, $7)
        RETURNING id
        "#,
        notification.user_id,
        notification.notification_type,
        notification.subject,
        notification.body,
        notification.channel.as_str(),
        notification.ref_type,
        notification.ref_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let notif_id = row.id;

    // Route to channel
    let new_status = match notification.channel {
        NotifyChannel::Email => match send_email_notification(state, &notification).await {
            Ok(()) => "sent",
            Err(e) => {
                tracing::error!(error = %e, notification_id = %notif_id, "email notification failed");
                "failed"
            }
        },
        // In-app: just stored (UI polls or WebSocket pushes).
        // Webhook: routed via fire_webhooks() from webhooks module; stored as sent here.
        NotifyChannel::InApp | NotifyChannel::Webhook => "sent",
    };

    // Update status
    let _ = sqlx::query!(
        "UPDATE notifications SET status = $1 WHERE id = $2",
        new_status,
        notif_id,
    )
    .execute(&state.pool)
    .await;

    Ok(())
}

/// Send email for a notification. Looks up the user's email.
async fn send_email_notification(
    state: &AppState,
    notification: &NewNotification,
) -> anyhow::Result<()> {
    let user = sqlx::query!(
        "SELECT email FROM users WHERE id = $1",
        notification.user_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("user not found for email notification"))?;

    let body_text = notification.body.as_deref().unwrap_or("");
    crate::notify::email::send(&state.config, &user.email, &notification.subject, body_text).await
}

// ---------------------------------------------------------------------------
// Event-driven helpers (callable by other modules)
// ---------------------------------------------------------------------------

/// Notify the project owner when a build fails.
pub async fn on_build_complete(state: &AppState, project_id: Uuid, status: &str) {
    if status != "failure" {
        return;
    }

    let owner = match sqlx::query!(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row.owner_id,
        _ => return,
    };

    let _ = notify(
        state,
        NewNotification {
            user_id: owner,
            notification_type: "build_failed".into(),
            subject: "Build failed".into(),
            body: Some(format!("A build in project {project_id} has failed.")),
            channel: NotifyChannel::InApp,
            ref_type: Some("pipeline".into()),
            ref_id: None,
        },
    )
    .await;
}

/// Notify when a merge request is created (stub — expand when reviewer logic exists).
pub async fn on_mr_created(state: &AppState, project_id: Uuid, mr_number: i64) {
    let owner = match sqlx::query!(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row.owner_id,
        _ => return,
    };

    let _ = notify(
        state,
        NewNotification {
            user_id: owner,
            notification_type: "mr_created".into(),
            subject: format!("New merge request #{mr_number}"),
            body: Some(format!(
                "A new merge request #{mr_number} was created in project {project_id}."
            )),
            channel: NotifyChannel::InApp,
            ref_type: Some("mr".into()),
            ref_id: None,
        },
    )
    .await;
}

/// Notify when a deploy completes.
pub async fn on_deploy_status(state: &AppState, project_id: Uuid, status: &str) {
    let owner = match sqlx::query!(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row.owner_id,
        _ => return,
    };

    let _ = notify(
        state,
        NewNotification {
            user_id: owner,
            notification_type: "deploy_status".into(),
            subject: format!("Deployment {status}"),
            body: Some(format!(
                "A deployment in project {project_id} completed with status: {status}."
            )),
            channel: NotifyChannel::InApp,
            ref_type: Some("deployment".into()),
            ref_id: None,
        },
    )
    .await;
}

/// Notify when an agent session completes.
pub async fn on_agent_completed(state: &AppState, user_id: Uuid, session_id: Uuid) {
    let _ = notify(
        state,
        NewNotification {
            user_id,
            notification_type: "agent_completed".into(),
            subject: "Agent session completed".into(),
            body: Some(format!("Agent session {session_id} has completed.")),
            channel: NotifyChannel::InApp,
            ref_type: Some("session".into()),
            ref_id: Some(session_id),
        },
    )
    .await;
}

/// Notify when an alert fires.
pub async fn on_alert_firing(state: &AppState, user_id: Uuid, alert_id: Uuid) {
    let _ = notify(
        state,
        NewNotification {
            user_id,
            notification_type: "alert_firing".into(),
            subject: "Alert firing".into(),
            body: Some(format!("Alert {alert_id} is firing.")),
            channel: NotifyChannel::InApp,
            ref_type: Some("alert".into()),
            ref_id: Some(alert_id),
        },
    )
    .await;
}
