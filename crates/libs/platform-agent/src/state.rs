// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Crate-local state for agent operations.

use std::sync::Arc;

use sqlx::PgPool;

use crate::claude_cli::session::CliSessionManager;
use crate::config::AgentConfig;

/// Shared state for all agent operations.
///
/// Constructed from the main binary's `AppState` via `AppState::agent_state()`.
/// Webhook dispatch is handled via `PlatformEvent::AgentSessionEnded` events
/// published to Valkey, handled by the eventbus.
#[derive(Clone)]
pub struct AgentState {
    pub pool: PgPool,
    pub valkey: fred::clients::Pool,
    pub kube: kube::Client,
    pub minio: opendal::Operator,
    pub config: Arc<AgentConfig>,
    pub cli_sessions: CliSessionManager,
    pub task_registry: Arc<dyn platform_types::TaskHeartbeat>,
}
