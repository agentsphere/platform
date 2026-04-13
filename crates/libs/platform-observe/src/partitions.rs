// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Background task that creates future monthly partitions for observe tables.
//!
//! Runs daily and ensures partitions exist for the next 3 months. Tables
//! were initially partitioned in migration 030004 with partitions through
//! September 2026; this task extends coverage beyond that.

use chrono::{Datelike, Utc};
use sqlx::PgPool;
use std::time::Duration;

/// Tables partitioned by time range with their partition key column.
const PARTITIONED_TABLES: &[&str] = &["spans", "log_entries", "metric_samples"];

/// How many months ahead to create partitions.
const MONTHS_AHEAD: u32 = 3;

/// Run the partition manager loop until shutdown.
pub async fn run(pool: PgPool, cancel: tokio_util::sync::CancellationToken) {
    // Run once immediately at startup, then daily.
    let mut interval = tokio::time::interval(Duration::from_secs(86_400));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = ensure_partitions(&pool).await {
                    tracing::warn!(error = %e, "partition management failed");
                }
            }
            () = cancel.cancelled() => break,
        }
    }
}

/// Create monthly partitions for the next `MONTHS_AHEAD` months if they don't exist.
async fn ensure_partitions(pool: &PgPool) -> Result<(), sqlx::Error> {
    let now = Utc::now();

    for &table in PARTITIONED_TABLES {
        for offset in 0..=MONTHS_AHEAD {
            let (year, month) = add_months(now.year(), now.month(), offset);
            let partition_name = format!("{table}_p_{year}{month:02}");

            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = $1 AND relkind = 'r')",
            )
            .bind(&partition_name)
            .fetch_one(pool)
            .await?;

            if !exists {
                let (end_year, end_month) = add_months(year, month, 1);
                let start = format!("{year}-{month:02}-01");
                let end = format!("{end_year}-{end_month:02}-01");

                let sql = format!(
                    "CREATE TABLE {partition_name} PARTITION OF {table} \
                     FOR VALUES FROM ('{start}') TO ('{end}')"
                );
                sqlx::query(&sql).execute(pool).await?;
                tracing::info!(table, partition = %partition_name, "created partition");
            }
        }
    }

    Ok(())
}

/// Add `add` months to a (year, month) pair, handling year rollover.
fn add_months(year: i32, month: u32, add: u32) -> (i32, u32) {
    let total = u32::try_from(year).unwrap_or(0) * 12 + month - 1 + add;
    (i32::try_from(total / 12).unwrap_or(year), total % 12 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_months_same_year() {
        assert_eq!(add_months(2026, 4, 0), (2026, 4));
        assert_eq!(add_months(2026, 4, 1), (2026, 5));
        assert_eq!(add_months(2026, 4, 8), (2026, 12));
    }

    #[test]
    fn add_months_year_rollover() {
        assert_eq!(add_months(2026, 4, 9), (2027, 1));
        assert_eq!(add_months(2026, 12, 1), (2027, 1));
        assert_eq!(add_months(2026, 12, 12), (2027, 12));
        assert_eq!(add_months(2026, 12, 13), (2028, 1));
    }

    #[test]
    fn partition_name_format() {
        let (year, month) = add_months(2026, 4, 0);
        let name = format!("spans_p_{year}{month:02}");
        assert_eq!(name, "spans_p_202604");
    }
}
