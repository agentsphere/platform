// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Focused state for the observe subsystem — no dependency on main binary's `AppState`.

use sqlx::PgPool;

/// Shared state for the observe subsystem.
#[derive(Clone)]
pub struct ObserveState {
    pub pool: PgPool,
    pub valkey: fred::clients::Pool,
    pub minio: opendal::Operator,
    pub config: ObserveConfig,
}

/// Configuration for the observe subsystem.
#[derive(Clone)]
pub struct ObserveConfig {
    /// How many days of data to retain before purging.
    pub retention_days: u32,
    /// Channel buffer capacity per signal type.
    pub buffer_capacity: usize,
    /// Hours of log data to keep in Postgres before rotating to Parquet.
    pub parquet_log_retention_hours: u32,
    /// Hours of metric data to keep in Postgres before rotating to Parquet.
    pub parquet_metric_retention_hours: u32,
    /// Whether to trust X-Forwarded-For for client IP extraction.
    pub trust_proxy: bool,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            retention_days: 30,
            buffer_capacity: 10_000,
            parquet_log_retention_hours: 48,
            parquet_metric_retention_hours: 1,
            trust_proxy: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_sensible_values() {
        let cfg = ObserveConfig::default();
        assert_eq!(cfg.retention_days, 30);
        assert_eq!(cfg.buffer_capacity, 10_000);
        assert_eq!(cfg.parquet_log_retention_hours, 48);
        assert_eq!(cfg.parquet_metric_retention_hours, 1);
        assert!(!cfg.trust_proxy);
    }
}
