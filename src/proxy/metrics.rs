//! RED metrics (Request rate, Error rate, Duration) and process metrics.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

/// Metric record for OTLP export.
#[derive(Debug, Clone)]
pub struct MetricRecord {
    pub name: String,
    pub labels: JsonValue,
    pub metric_type: String,
    pub unit: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub value: f64,
}

/// RED metric counters for HTTP proxy traffic.
pub struct RedMetrics {
    requests: AtomicU64,
    errors: AtomicU64,
    duration_sum_ms: AtomicU64,
    /// Histogram buckets: <5ms, <10ms, <25ms, <50ms, <100ms, <250ms, <500ms,
    /// <1s, <5s, <10s, >10s
    buckets: [AtomicU64; 11],
}

/// Bucket boundaries in milliseconds for the duration histogram.
pub const BUCKET_BOUNDS_MS: [u64; 10] = [5, 10, 25, 50, 100, 250, 500, 1000, 5000, 10000];

impl RedMetrics {
    /// Create a new zeroed `RedMetrics`.
    pub fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            duration_sum_ms: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Record a completed request.
    pub fn record(&self, duration_ms: u64, is_error: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if is_error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.duration_sum_ms
            .fetch_add(duration_ms, Ordering::Relaxed);

        // Find the correct histogram bucket
        let bucket_idx = BUCKET_BOUNDS_MS
            .iter()
            .position(|&bound| duration_ms < bound)
            .unwrap_or(BUCKET_BOUNDS_MS.len());
        self.buckets[bucket_idx].fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot and reset all counters. Returns current values.
    pub fn snapshot_and_reset(&self) -> RedSnapshot {
        RedSnapshot {
            requests: self.requests.swap(0, Ordering::Relaxed),
            errors: self.errors.swap(0, Ordering::Relaxed),
            duration_sum_ms: self.duration_sum_ms.swap(0, Ordering::Relaxed),
            buckets: std::array::from_fn(|i| self.buckets[i].swap(0, Ordering::Relaxed)),
        }
    }
}

impl Default for RedMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of RED metrics at a point in time.
#[derive(Debug)]
pub struct RedSnapshot {
    pub requests: u64,
    pub errors: u64,
    pub duration_sum_ms: u64,
    pub buckets: [u64; 11],
}

impl RedSnapshot {
    /// Convert this snapshot to a vector of `MetricRecord` for OTLP export.
    pub fn to_metric_records(&self, service: &str) -> Vec<MetricRecord> {
        let now = Utc::now();
        let labels = serde_json::json!({"service": service});
        let mut records = Vec::with_capacity(4);

        records.push(MetricRecord {
            name: "http.server.request.count".into(),
            labels: labels.clone(),
            metric_type: "sum".into(),
            unit: Some("{request}".into()),
            timestamp: now,
            #[allow(clippy::cast_precision_loss)]
            value: self.requests as f64,
        });

        records.push(MetricRecord {
            name: "http.server.error.count".into(),
            labels: labels.clone(),
            metric_type: "sum".into(),
            unit: Some("{request}".into()),
            timestamp: now,
            #[allow(clippy::cast_precision_loss)]
            value: self.errors as f64,
        });

        records.push(MetricRecord {
            name: "http.server.request.duration_sum".into(),
            labels: labels.clone(),
            metric_type: "sum".into(),
            unit: Some("ms".into()),
            timestamp: now,
            #[allow(clippy::cast_precision_loss)]
            value: self.duration_sum_ms as f64,
        });

        // Histogram bucket counts
        for (i, &count) in self.buckets.iter().enumerate() {
            let bound_label = if i < BUCKET_BOUNDS_MS.len() {
                format!("le_{}", BUCKET_BOUNDS_MS[i])
            } else {
                "le_inf".to_string()
            };
            let mut bucket_labels = serde_json::Map::new();
            bucket_labels.insert("service".into(), serde_json::Value::String(service.into()));
            bucket_labels.insert("bucket".into(), serde_json::Value::String(bound_label));
            records.push(MetricRecord {
                name: "http.server.request.duration_bucket".into(),
                labels: serde_json::Value::Object(bucket_labels),
                metric_type: "gauge".into(),
                unit: Some("ms".into()),
                timestamp: now,
                #[allow(clippy::cast_precision_loss)]
                value: count as f64,
            });
        }

        records
    }
}

/// Flush RED metrics to the metric channel at the given interval.
#[tracing::instrument(skip_all)]
pub async fn flush_red_metrics(
    red: std::sync::Arc<RedMetrics>,
    service: String,
    metric_tx: mpsc::Sender<MetricRecord>,
    interval: std::time::Duration,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let snap = red.snapshot_and_reset();
                if snap.requests > 0 {
                    for record in snap.to_metric_records(&service) {
                        let _ = metric_tx.try_send(record);
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn red_metrics_record_and_snapshot() {
        let m = RedMetrics::new();
        m.record(3, false); // bucket 0 (<5ms)
        m.record(50, false); // bucket 4 (<100ms)
        m.record(100, true); // bucket 4 (<250ms)

        let snap = m.snapshot_and_reset();
        assert_eq!(snap.requests, 3);
        assert_eq!(snap.errors, 1);
        assert_eq!(snap.duration_sum_ms, 153);
        // 3ms → bucket <5ms (index 0)
        assert_eq!(snap.buckets[0], 1);
        // 50ms → bucket <100ms (index 4, bounds: [5,10,25,50,100,...])
        assert_eq!(snap.buckets[4], 1);
        // 100ms → bucket <250ms (index 5)
        assert_eq!(snap.buckets[5], 1);
    }

    #[test]
    fn red_metrics_snapshot_resets() {
        let m = RedMetrics::new();
        m.record(10, false);
        let snap1 = m.snapshot_and_reset();
        assert_eq!(snap1.requests, 1);

        let snap2 = m.snapshot_and_reset();
        assert_eq!(snap2.requests, 0);
    }

    #[test]
    fn red_snapshot_to_records() {
        let m = RedMetrics::new();
        m.record(5, false);
        m.record(500, true);

        let snap = m.snapshot_and_reset();
        let records = snap.to_metric_records("test-svc");
        // 3 summary metrics + 11 bucket metrics
        assert_eq!(records.len(), 14);

        let count_record = records
            .iter()
            .find(|r| r.name == "http.server.request.count");
        assert!(count_record.is_some());
        assert!((count_record.unwrap().value - 2.0).abs() < f64::EPSILON);

        let error_record = records.iter().find(|r| r.name == "http.server.error.count");
        assert!(error_record.is_some());
        assert!((error_record.unwrap().value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bucket_overflow() {
        let m = RedMetrics::new();
        m.record(20000, false); // >10s, should go to last bucket (index 10)
        let snap = m.snapshot_and_reset();
        assert_eq!(snap.buckets[10], 1);
        // All other buckets should be 0
        for i in 0..10 {
            assert_eq!(snap.buckets[i], 0);
        }
    }

    #[test]
    fn boundary_values() {
        let m = RedMetrics::new();
        // Exactly on boundary: 5ms goes into <10ms bucket (index 1)
        m.record(5, false);
        let snap = m.snapshot_and_reset();
        assert_eq!(snap.buckets[1], 1);
    }
}
