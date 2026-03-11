use fred::interfaces::ClientLike;
use fred::interfaces::EventInterface;
use fred::interfaces::PubsubInterface;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::provider::ProgressEvent;
use super::valkey_acl;

/// Publish a `ProgressEvent` to the session's events channel.
/// Used by server-side code (create-app tool loop) to emit events.
#[allow(dead_code)] // Used by Step 5 (create-app rewrite)
#[tracing::instrument(skip(valkey, event), fields(%session_id), err)]
pub async fn publish_event(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    event: &ProgressEvent,
) -> Result<(), anyhow::Error> {
    let channel = valkey_acl::events_channel(session_id);
    let json = serde_json::to_string(event)?;
    valkey.next().publish::<(), _, _>(&channel, json).await?;
    Ok(())
}

/// Publish a user prompt to the session's input channel.
#[tracing::instrument(skip(valkey, content), fields(%session_id), err)]
pub async fn publish_prompt(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    content: &str,
) -> Result<(), anyhow::Error> {
    publish_prompt_with_source(valkey, session_id, content, "user").await
}

/// Publish a prompt with a source identifier to a session's input channel.
/// Used by Manager → Worker communication (`source="manager"`) and user messages (`source="user"`).
#[tracing::instrument(skip(valkey, content), fields(%session_id, %source), err)]
pub async fn publish_prompt_with_source(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    content: &str,
    source: &str,
) -> Result<(), anyhow::Error> {
    let channel = valkey_acl::input_channel(session_id);
    let msg = serde_json::json!({ "type": "prompt", "content": content, "source": source });
    valkey
        .next()
        .publish::<(), _, _>(&channel, msg.to_string())
        .await?;
    Ok(())
}

/// Publish a control message (e.g., interrupt) to the session's input channel.
#[allow(dead_code)] // Used by Step 5 (create-app rewrite)
#[tracing::instrument(skip(valkey), fields(%session_id, %control_type), err)]
pub async fn publish_control(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    control_type: &str,
) -> Result<(), anyhow::Error> {
    let channel = valkey_acl::input_channel(session_id);
    let msg = serde_json::json!({ "type": "control", "control_type": control_type });
    valkey
        .next()
        .publish::<(), _, _>(&channel, msg.to_string())
        .await?;
    Ok(())
}

/// Spawn a background task that subscribes to session events and persists them to `agent_messages`.
/// Started at session creation. Exits on Completed/Error events.
///
/// The returned future resolves once the Valkey subscription is established,
/// ensuring no pub/sub messages are lost to a race condition.
pub async fn spawn_persistence_subscriber(
    pool: PgPool,
    valkey: fred::clients::Pool,
    session_id: Uuid,
) -> JoinHandle<()> {
    let channel = valkey_acl::events_channel(session_id);

    // Subscribe on a dedicated connection before spawning the background task.
    // This ensures the subscription is active before any pod publishes events.
    // clone_new() creates an unconnected client — init() establishes the connection.
    let subscriber = valkey.next().clone_new();
    if let Err(e) = subscriber.init().await {
        tracing::error!(error = %e, %session_id, "persistence subscriber failed to init");
        return tokio::spawn(async {});
    }
    if let Err(e) = subscriber.subscribe(&channel).await {
        tracing::error!(error = %e, %session_id, "persistence subscriber failed to subscribe");
        return tokio::spawn(async {});
    }
    let rx = subscriber.message_rx();

    tokio::spawn(async move {
        if let Err(e) = run_persistence_subscriber(pool, subscriber, rx, channel, session_id).await
        {
            tracing::error!(error = %e, %session_id, "persistence subscriber failed");
        }
    })
}

async fn run_persistence_subscriber(
    pool: PgPool,
    subscriber: fred::clients::Client,
    mut rx: tokio::sync::broadcast::Receiver<fred::types::Message>,
    channel: String,
    session_id: Uuid,
) -> Result<(), anyhow::Error> {
    while let Ok(msg) = rx.recv().await {
        let payload: String = match msg.value.convert() {
            Ok(s) => s,
            Err(_) => continue,
        };

        let event: ProgressEvent = match serde_json::from_str(&payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, %session_id, "malformed event in pub/sub, skipping");
                continue;
            }
        };

        // Persist to agent_messages
        let kind_str = serde_json::to_string(&event.kind).unwrap_or_default();
        // Remove surrounding quotes from the JSON string
        let kind_str = kind_str.trim_matches('"');
        let _ = sqlx::query(
            "INSERT INTO agent_messages (session_id, role, content, metadata)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(session_id)
        .bind(kind_str)
        .bind(&event.message)
        .bind(&event.metadata)
        .execute(&pool)
        .await;

        // Exit on terminal events
        if matches!(
            event.kind,
            super::provider::ProgressKind::Completed | super::provider::ProgressKind::Error
        ) {
            break;
        }
    }

    let _ = subscriber.unsubscribe(&channel).await;
    tracing::debug!(%session_id, "persistence subscriber exited");
    Ok(())
}

/// Subscribe to session events for SSE streaming. Returns an `mpsc::Receiver`.
/// Read-only — does NOT write to DB. SSE handler wraps this in `Sse<impl Stream>`.
pub async fn subscribe_session_events(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> Result<mpsc::Receiver<ProgressEvent>, anyhow::Error> {
    let channel = valkey_acl::events_channel(session_id);
    let (tx, rx) = mpsc::channel(256);

    // Dedicated subscriber connection (separate from pool command connections).
    // clone_new() creates an unconnected client — init() establishes the connection.
    let subscriber = valkey.next().clone_new();
    subscriber.init().await?;
    subscriber.subscribe(&channel).await?;
    let mut msg_rx = subscriber.message_rx();

    tokio::spawn(async move {
        while let Ok(msg) = msg_rx.recv().await {
            let payload: String = match msg.value.convert() {
                Ok(s) => s,
                Err(_) => continue,
            };

            if let Ok(event) = serde_json::from_str::<ProgressEvent>(&payload) {
                let is_terminal = matches!(
                    event.kind,
                    super::provider::ProgressKind::Completed | super::provider::ProgressKind::Error
                );
                if tx.send(event).await.is_err() {
                    // Receiver dropped — unsubscribe and exit
                    break;
                }
                if is_terminal {
                    break;
                }
            }
        }

        let _ = subscriber.unsubscribe(&channel).await;
    });

    Ok(rx)
}

/// Subscribe to events from multiple sessions (parent + children).
/// Returns `(Uuid, ProgressEvent)` tuples so the caller knows which session emitted each event.
/// Exits when ALL sessions have emitted a terminal event (Completed/Error), or when the
/// receiver is dropped.
pub async fn subscribe_session_tree_events(
    valkey: &fred::clients::Pool,
    session_ids: &[Uuid],
) -> Result<mpsc::Receiver<(Uuid, ProgressEvent)>, anyhow::Error> {
    let (tx, rx) = mpsc::channel(256);

    if session_ids.is_empty() {
        return Ok(rx);
    }

    // Build channel → session_id mapping
    let channels: Vec<String> = session_ids
        .iter()
        .map(|id| valkey_acl::events_channel(*id))
        .collect();
    let channel_to_session: std::collections::HashMap<String, Uuid> = channels
        .iter()
        .zip(session_ids.iter())
        .map(|(ch, id)| (ch.clone(), *id))
        .collect();

    // Single dedicated subscriber for all channels
    let subscriber = valkey.next().clone_new();
    subscriber.init().await?;
    for ch in &channels {
        subscriber.subscribe(ch).await?;
    }
    let mut msg_rx = subscriber.message_rx();

    let channel_list = channels.clone();
    let terminal_count = session_ids.len();

    tokio::spawn(async move {
        let mut terminals_seen = 0usize;

        while let Ok(msg) = msg_rx.recv().await {
            let payload: String = match msg.value.convert() {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Extract session_id from channel name
            let channel_name = msg.channel.to_string();
            let session_id = match channel_to_session.get(&channel_name) {
                Some(id) => *id,
                None => continue,
            };

            if let Ok(event) = serde_json::from_str::<ProgressEvent>(&payload) {
                let is_terminal = matches!(
                    event.kind,
                    super::provider::ProgressKind::Completed | super::provider::ProgressKind::Error
                );
                if tx.send((session_id, event)).await.is_err() {
                    break;
                }
                if is_terminal {
                    terminals_seen += 1;
                    if terminals_seen >= terminal_count {
                        break;
                    }
                }
            }
        }

        for ch in &channel_list {
            let _ = subscriber.unsubscribe(ch).await;
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::provider::{ProgressEvent, ProgressKind};

    #[test]
    fn test_channel_names_correct() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            valkey_acl::events_channel(id),
            "session:550e8400-e29b-41d4-a716-446655440000:events"
        );
        assert_eq!(
            valkey_acl::input_channel(id),
            "session:550e8400-e29b-41d4-a716-446655440000:input"
        );
    }

    #[test]
    fn test_publish_event_serialization() {
        let event = ProgressEvent {
            kind: ProgressKind::Text,
            message: "hello world".into(),
            metadata: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"text\""));
        assert!(json.contains("\"message\":\"hello world\""));
    }

    #[test]
    fn test_prompt_json_format() {
        let msg = serde_json::json!({ "type": "prompt", "content": "test prompt" });
        assert_eq!(msg["type"], "prompt");
        assert_eq!(msg["content"], "test prompt");
    }

    #[test]
    fn test_prompt_with_source_json_format() {
        let msg =
            serde_json::json!({ "type": "prompt", "content": "do this", "source": "manager" });
        assert_eq!(msg["type"], "prompt");
        assert_eq!(msg["content"], "do this");
        assert_eq!(msg["source"], "manager");
    }

    #[test]
    fn test_prompt_with_user_source_json_format() {
        let msg = serde_json::json!({ "type": "prompt", "content": "hello", "source": "user" });
        assert_eq!(msg["source"], "user");
    }

    #[test]
    fn test_control_json_format() {
        let msg = serde_json::json!({ "type": "control", "control_type": "interrupt" });
        assert_eq!(msg["type"], "control");
        assert_eq!(msg["control_type"], "interrupt");
    }

    #[test]
    fn test_event_deserialization_for_persistence() {
        let json = r#"{"kind":"completed","message":"done","metadata":{"cost":0.01}}"#;
        let event: ProgressEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.kind, ProgressKind::Completed);
        assert!(event.metadata.is_some());
    }

    #[test]
    fn test_malformed_event_skipped() {
        let result = serde_json::from_str::<ProgressEvent>("not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_kind_serialized_for_db() {
        let kind = ProgressKind::ToolCall;
        let json = serde_json::to_string(&kind).unwrap();
        let trimmed = json.trim_matches('"');
        assert_eq!(trimmed, "tool_call");
    }

    #[test]
    fn test_event_with_metadata_round_trip() {
        let event = ProgressEvent {
            kind: ProgressKind::ToolCall,
            message: "Read file".into(),
            metadata: Some(serde_json::json!({"file": "test.rs"})),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ProgressEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, ProgressKind::ToolCall);
        assert_eq!(back.message, "Read file");
        assert!(back.metadata.is_some());
    }

    #[test]
    fn test_event_without_metadata_serializes_clean() {
        let event = ProgressEvent {
            kind: ProgressKind::Text,
            message: "test".into(),
            metadata: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("metadata"),
            "metadata should be skipped when None"
        );
    }

    #[test]
    fn test_tree_channel_mapping() {
        let id1 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let id2 = Uuid::parse_str("660e8400-e29b-41d4-a716-446655440000").unwrap();
        let ids = [id1, id2];

        let channels: Vec<String> = ids
            .iter()
            .map(|id| valkey_acl::events_channel(*id))
            .collect();
        let map: std::collections::HashMap<String, Uuid> = channels
            .iter()
            .zip(ids.iter())
            .map(|(ch, id)| (ch.clone(), *id))
            .collect();

        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("session:550e8400-e29b-41d4-a716-446655440000:events"),
            Some(&id1)
        );
        assert_eq!(
            map.get("session:660e8400-e29b-41d4-a716-446655440000:events"),
            Some(&id2)
        );
    }

    #[test]
    fn test_tree_empty_ids_returns_empty_channel_list() {
        let ids: Vec<Uuid> = vec![];
        let channels: Vec<String> = ids
            .iter()
            .map(|id| valkey_acl::events_channel(*id))
            .collect();
        assert!(channels.is_empty());
    }
}
