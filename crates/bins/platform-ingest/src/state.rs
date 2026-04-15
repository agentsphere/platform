// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Shared state for the ingest binary.
#[derive(Clone)]
pub struct IngestState {
    pub pool: sqlx::PgPool,
    pub valkey: fred::clients::Pool,
    pub trust_proxy: bool,
    /// Set to `true` when the alert router fails to load/rebuild.
    /// Reset to `false` on successful rebuild.
    pub alert_router_degraded: Arc<AtomicBool>,
}
