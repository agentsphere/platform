//! Metrics scraping: Prometheus, Postgres stats, Redis/Valkey INFO.
//!
//! Replaces `OTel` collector sidecars by scraping metrics from infra services
//! directly and converting to OTLP format.

use std::time::Duration;

use chrono::Utc;
use tokio::sync::{mpsc, watch};

use super::metrics::MetricRecord;

/// Scraper configuration.
#[derive(Debug, Clone)]
pub struct ScraperConfig {
    pub scrape_type: Option<String>,
    pub scrape_url: Option<String>,
    pub postgres_url: Option<String>,
    pub redis_url: Option<String>,
    pub tls_insecure: bool,
    pub service_name: String,
}

/// Run the metrics scraper background task.
///
/// Periodically scrapes metrics from the configured source and sends
/// `MetricRecord`s to the metric channel for OTLP export.
#[tracing::instrument(skip_all, fields(
    scrape_type = ?config.scrape_type,
    service = %config.service_name,
))]
pub async fn run_metrics_scraper(
    config: ScraperConfig,
    metric_tx: mpsc::Sender<MetricRecord>,
    interval: Duration,
    mut shutdown: watch::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    // Skip the first tick (immediate)
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let metrics = match scrape_once(&config).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "scrape failed");
                        continue;
                    }
                };
                for m in metrics {
                    let _ = metric_tx.try_send(m);
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("metrics scraper exiting");
}

/// Perform a single scrape iteration.
async fn scrape_once(config: &ScraperConfig) -> anyhow::Result<Vec<MetricRecord>> {
    // Prometheus scrape URL takes priority
    if let Some(ref url) = config.scrape_url {
        return scrape_prometheus(url, &config.service_name, config.tls_insecure).await;
    }

    match config.scrape_type.as_deref() {
        Some("postgres") => {
            let url = config
                .postgres_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("PROXY_SCRAPE_POSTGRES_URL not set"))?;
            scrape_postgres(url, &config.service_name).await
        }
        Some("redis") => {
            let url = config
                .redis_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("PROXY_SCRAPE_REDIS_URL not set"))?;
            scrape_redis(url, &config.service_name).await
        }
        Some(other) => {
            anyhow::bail!("unknown scrape type: {other}");
        }
        None => Ok(Vec::new()),
    }
}

// ---------------------------------------------------------------------------
// Prometheus scraper
// ---------------------------------------------------------------------------

/// Scrape a Prometheus text exposition format endpoint.
///
/// Lines starting with `#` are comments/type hints. Metric lines:
/// `name{label="value",...} value [timestamp]`
#[tracing::instrument(skip_all)]
async fn scrape_prometheus(
    url: &str,
    service: &str,
    tls_insecure: bool,
) -> anyhow::Result<Vec<MetricRecord>> {
    let client = if tls_insecure {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(10))
            .build()?
    } else {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?
    };

    let text = client.get(url).send().await?.text().await?;

    Ok(parse_prometheus_text(&text, service))
}

/// Parse Prometheus text exposition format into `MetricRecord`s.
pub fn parse_prometheus_text(text: &str, service: &str) -> Vec<MetricRecord> {
    let now = Utc::now();
    let mut records = Vec::new();
    let mut metric_types: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse TYPE comments
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() == 2 {
                metric_types.insert(parts[0].to_string(), parts[1].to_string());
            }
            continue;
        }

        // Skip other comments
        if line.starts_with('#') {
            continue;
        }

        // Parse metric line: name{labels} value [timestamp]
        if let Some(record) = parse_prometheus_metric_line(line, service, &metric_types, now) {
            records.push(record);
        }
    }

    records
}

/// Parse a single Prometheus metric line.
fn parse_prometheus_metric_line(
    line: &str,
    service: &str,
    metric_types: &std::collections::HashMap<String, String>,
    now: chrono::DateTime<Utc>,
) -> Option<MetricRecord> {
    // Split: name{labels} value [timestamp]
    let (name_and_labels, rest) = if let Some(brace_start) = line.find('{') {
        let brace_end = line.find('}')?;
        let name = &line[..brace_start];
        let labels_str = &line[brace_start + 1..brace_end];
        let value_part = line[brace_end + 1..].trim();
        let mut labels = serde_json::Map::new();
        labels.insert("service".into(), serde_json::Value::String(service.into()));
        for pair in labels_str.split(',') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                let v = v.trim_matches('"');
                labels.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            }
        }
        (
            (name.to_string(), serde_json::Value::Object(labels)),
            value_part,
        )
    } else {
        // No labels
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            return None;
        }
        let mut labels = serde_json::Map::new();
        labels.insert("service".into(), serde_json::Value::String(service.into()));
        (
            (parts[0].to_string(), serde_json::Value::Object(labels)),
            parts.get(1).copied().unwrap_or("0"),
        )
    };

    let value: f64 = rest.split_whitespace().next()?.parse().ok()?;

    let metric_type = metric_types
        .get(&name_and_labels.0)
        .cloned()
        .unwrap_or_else(|| "gauge".into());

    Some(MetricRecord {
        name: name_and_labels.0,
        labels: name_and_labels.1,
        metric_type,
        unit: None,
        timestamp: now,
        value,
    })
}

// ---------------------------------------------------------------------------
// Postgres scraper
// ---------------------------------------------------------------------------

/// Scrape Postgres statistics via SQL queries.
///
/// Queries `pg_stat_database`, `pg_stat_bgwriter`, `pg_stat_activity` to
/// produce metrics matching what the `OTel` collector was emitting.
#[tracing::instrument(skip_all)]
async fn scrape_postgres(url: &str, service: &str) -> anyhow::Result<Vec<MetricRecord>> {
    let pool = sqlx::PgPool::connect(url).await?;
    let now = Utc::now();
    let mut records = Vec::new();
    let labels = serde_json::json!({"service": service});

    // pg_stat_database — aggregate across all databases
    let rows = sqlx::query(
        "SELECT
            COALESCE(SUM(numbackends), 0) as backends,
            COALESCE(SUM(xact_commit), 0) as commits,
            COALESCE(SUM(xact_rollback), 0) as rollbacks,
            COALESCE(SUM(blks_read), 0) as blks_read,
            COALESCE(SUM(blks_hit), 0) as blks_hit,
            COALESCE(SUM(tup_returned), 0) as rows_returned,
            COALESCE(SUM(tup_fetched), 0) as rows_fetched,
            COALESCE(SUM(tup_inserted), 0) as rows_inserted,
            COALESCE(SUM(tup_updated), 0) as rows_updated,
            COALESCE(SUM(tup_deleted), 0) as rows_deleted,
            COALESCE(SUM(temp_files), 0) as temp_files,
            COALESCE(SUM(temp_bytes), 0) as temp_bytes,
            COALESCE(SUM(deadlocks), 0) as deadlocks
        FROM pg_stat_database",
    )
    .fetch_one(&pool)
    .await?;

    use sqlx::Row;
    let stat_metrics = [
        ("postgresql.backends", "backends", "gauge"),
        ("postgresql.commits", "commits", "sum"),
        ("postgresql.rollbacks", "rollbacks", "sum"),
        ("postgresql.blks_read", "blks_read", "sum"),
        ("postgresql.blks_hit", "blks_hit", "sum"),
        ("postgresql.rows_returned", "rows_returned", "sum"),
        ("postgresql.rows_fetched", "rows_fetched", "sum"),
        ("postgresql.rows_inserted", "rows_inserted", "sum"),
        ("postgresql.rows_updated", "rows_updated", "sum"),
        ("postgresql.rows_deleted", "rows_deleted", "sum"),
        ("postgresql.temp_files", "temp_files", "sum"),
        ("postgresql.temp_bytes", "temp_bytes", "sum"),
        ("postgresql.deadlocks", "deadlocks", "sum"),
    ];

    for (name, col, mtype) in &stat_metrics {
        let value: i64 = rows.try_get(*col).unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        let fval = value as f64;
        records.push(MetricRecord {
            name: (*name).to_string(),
            labels: labels.clone(),
            metric_type: (*mtype).to_string(),
            unit: None,
            timestamp: now,
            value: fval,
        });
    }

    append_pg_db_sizes(&pool, service, now, &mut records).await?;
    append_pg_connections(&pool, service, now, &mut records).await?;
    pool.close().await;
    Ok(records)
}

/// Collect per-database sizes.
#[tracing::instrument(skip_all)]
async fn append_pg_db_sizes(
    pool: &sqlx::PgPool,
    service: &str,
    now: chrono::DateTime<Utc>,
    records: &mut Vec<MetricRecord>,
) -> anyhow::Result<()> {
    use sqlx::Row;
    let size_rows = sqlx::query(
        "SELECT datname, pg_database_size(datname) as db_size
         FROM pg_database WHERE datistemplate = false",
    )
    .fetch_all(pool)
    .await?;

    for row in &size_rows {
        let db_name: &str = row.try_get("datname").unwrap_or("unknown");
        let db_size: i64 = row.try_get("db_size").unwrap_or(0);
        let mut db_labels = serde_json::Map::new();
        db_labels.insert("service".into(), serde_json::Value::String(service.into()));
        db_labels.insert(
            "database".into(),
            serde_json::Value::String(db_name.to_string()),
        );
        #[allow(clippy::cast_precision_loss)]
        let fval = db_size as f64;
        records.push(MetricRecord {
            name: "postgresql.db_size".into(),
            labels: serde_json::Value::Object(db_labels),
            metric_type: "gauge".into(),
            unit: Some("bytes".into()),
            timestamp: now,
            value: fval,
        });
    }
    Ok(())
}

/// Collect connection count by state.
#[tracing::instrument(skip_all)]
async fn append_pg_connections(
    pool: &sqlx::PgPool,
    service: &str,
    now: chrono::DateTime<Utc>,
    records: &mut Vec<MetricRecord>,
) -> anyhow::Result<()> {
    use sqlx::Row;
    let state_rows = sqlx::query(
        "SELECT state, COUNT(*) as count
         FROM pg_stat_activity
         WHERE state IS NOT NULL
         GROUP BY state",
    )
    .fetch_all(pool)
    .await?;

    for row in &state_rows {
        let state: &str = row.try_get("state").unwrap_or("unknown");
        let count: i64 = row.try_get("count").unwrap_or(0);
        let mut state_labels = serde_json::Map::new();
        state_labels.insert("service".into(), serde_json::Value::String(service.into()));
        state_labels.insert("state".into(), serde_json::Value::String(state.to_string()));
        #[allow(clippy::cast_precision_loss)]
        let fval = count as f64;
        records.push(MetricRecord {
            name: "postgresql.connections".into(),
            labels: serde_json::Value::Object(state_labels),
            metric_type: "gauge".into(),
            unit: None,
            timestamp: now,
            value: fval,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Redis/Valkey scraper
// ---------------------------------------------------------------------------

/// Scrape Redis/Valkey via the INFO command over raw TCP.
#[tracing::instrument(skip_all)]
async fn scrape_redis(url: &str, service: &str) -> anyhow::Result<Vec<MetricRecord>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Parse redis://host:port or just host:port
    let addr = if let Some(rest) = url.strip_prefix("redis://") {
        rest.trim_end_matches('/')
    } else {
        url
    };

    let mut stream = tokio::net::TcpStream::connect(addr).await?;

    // Send INFO command using RESP protocol
    stream.write_all(b"*1\r\n$4\r\nINFO\r\n").await?;

    // Read response
    let mut buf = Vec::with_capacity(16384);
    let mut temp = [0u8; 8192];
    loop {
        let n = stream.read(&mut temp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[..n]);
        // INFO response ends with an empty line
        if buf.ends_with(b"\r\n\r\n") || buf.len() > 65536 {
            break;
        }
    }

    let info_text = String::from_utf8_lossy(&buf);
    Ok(parse_redis_info(&info_text, service))
}

/// Parse Redis INFO output into `MetricRecord`s.
pub fn parse_redis_info(info: &str, service: &str) -> Vec<MetricRecord> {
    let now = Utc::now();
    let labels = serde_json::json!({"service": service});
    let mut records = Vec::new();

    // Parse key:value pairs from INFO output
    let kv = parse_redis_kv(info);

    // Table-driven metric extraction: (metric_name, redis_key, type, unit)
    let gauge_bytes = [
        ("redis.memory.used", "used_memory"),
        ("redis.memory.rss", "used_memory_rss"),
        ("redis.memory.peak", "used_memory_peak"),
        ("redis.memory.lua", "used_memory_lua"),
    ];
    for (name, key) in &gauge_bytes {
        if let Some(val) = kv.get(*key).and_then(|v| v.parse::<f64>().ok()) {
            records.push(MetricRecord {
                name: (*name).to_string(),
                labels: labels.clone(),
                metric_type: "gauge".into(),
                unit: Some("bytes".into()),
                timestamp: now,
                value: val,
            });
        }
    }

    let gauges = [
        ("redis.clients.connected", "connected_clients"),
        ("redis.clients.blocked", "blocked_clients"),
        ("redis.replication.connected_slaves", "connected_slaves"),
        (
            "redis.rdb.changes_since_last_save",
            "rdb_changes_since_last_save",
        ),
    ];
    for (name, key) in &gauges {
        push_metric_if_present(&kv, key, name, "gauge", None, &labels, now, &mut records);
    }

    let counters = [
        ("redis.commands.processed", "total_commands_processed"),
        ("redis.connections.received", "total_connections_received"),
        ("redis.keys.expired", "expired_keys"),
        ("redis.keys.evicted", "evicted_keys"),
        ("redis.keyspace.hits", "keyspace_hits"),
        ("redis.keyspace.misses", "keyspace_misses"),
    ];
    for (name, key) in &counters {
        push_metric_if_present(&kv, key, name, "sum", None, &labels, now, &mut records);
    }

    // Uptime
    push_metric_if_present(
        &kv,
        "uptime_in_seconds",
        "redis.uptime",
        "gauge",
        Some("s"),
        &labels,
        now,
        &mut records,
    );

    records
}

/// Parse Redis INFO key:value lines into a map.
fn parse_redis_kv(info: &str) -> std::collections::HashMap<String, String> {
    let mut kv = std::collections::HashMap::new();
    for line in info.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('$') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            kv.insert(k.to_string(), v.to_string());
        }
    }
    kv
}

/// Push a metric record if the key exists and is parseable as f64.
#[allow(clippy::too_many_arguments)]
fn push_metric_if_present(
    kv: &std::collections::HashMap<String, String>,
    redis_key: &str,
    metric_name: &str,
    metric_type: &str,
    unit: Option<&str>,
    labels: &serde_json::Value,
    now: chrono::DateTime<Utc>,
    records: &mut Vec<MetricRecord>,
) {
    if let Some(val) = kv.get(redis_key).and_then(|v| v.parse::<f64>().ok()) {
        records.push(MetricRecord {
            name: metric_name.to_string(),
            labels: labels.clone(),
            metric_type: metric_type.to_string(),
            unit: unit.map(ToString::to_string),
            timestamp: now,
            value: val,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prometheus_basic() {
        let text = r#"# HELP minio_bucket_usage_total_bytes Total bucket size in bytes
# TYPE minio_bucket_usage_total_bytes gauge
minio_bucket_usage_total_bytes{bucket="test",server="localhost:9000"} 1024
minio_node_process_starttime_seconds 1.711234567e+09
"#;
        let records = parse_prometheus_text(text, "minio");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "minio_bucket_usage_total_bytes");
        assert!((records[0].value - 1024.0).abs() < f64::EPSILON);
        assert_eq!(records[1].name, "minio_node_process_starttime_seconds");
    }

    #[test]
    fn parse_prometheus_empty() {
        let records = parse_prometheus_text("", "svc");
        assert!(records.is_empty());
    }

    #[test]
    fn parse_prometheus_comments_only() {
        let text = "# HELP test\n# TYPE test gauge\n";
        let records = parse_prometheus_text(text, "svc");
        assert!(records.is_empty());
    }

    #[test]
    fn parse_prometheus_multiple_labels() {
        let text = r#"http_requests_total{method="POST",code="200"} 42"#;
        let records = parse_prometheus_text(text, "api");
        assert_eq!(records.len(), 1);
        let labels = records[0].labels.as_object().unwrap();
        assert_eq!(labels.get("method").unwrap(), "POST");
        assert_eq!(labels.get("code").unwrap(), "200");
        assert_eq!(labels.get("service").unwrap(), "api");
    }

    #[test]
    fn parse_prometheus_scientific_notation() {
        let text = "process_start_time 1.711234567e+09\n";
        let records = parse_prometheus_text(text, "svc");
        assert_eq!(records.len(), 1);
        assert!(records[0].value > 1.0e9);
    }

    #[test]
    fn parse_redis_info_basic() {
        let info = r#"$1234
# Server
redis_version:7.0.0
uptime_in_seconds:12345

# Clients
connected_clients:10
blocked_clients:2

# Memory
used_memory:1048576
used_memory_rss:2097152
used_memory_peak:3145728
used_memory_lua:37888

# Stats
total_commands_processed:100000
total_connections_received:500
expired_keys:42
evicted_keys:0
keyspace_hits:99000
keyspace_misses:1000

# Replication
connected_slaves:0

# Persistence
rdb_changes_since_last_save:100

"#;
        let records = parse_redis_info(info, "valkey");
        assert!(!records.is_empty());

        // Check specific metrics
        let memory = records
            .iter()
            .find(|r| r.name == "redis.memory.used")
            .unwrap();
        assert!((memory.value - 1_048_576.0).abs() < f64::EPSILON);

        let clients = records
            .iter()
            .find(|r| r.name == "redis.clients.connected")
            .unwrap();
        assert!((clients.value - 10.0).abs() < f64::EPSILON);

        let commands = records
            .iter()
            .find(|r| r.name == "redis.commands.processed")
            .unwrap();
        assert!((commands.value - 100_000.0).abs() < f64::EPSILON);

        let uptime = records.iter().find(|r| r.name == "redis.uptime").unwrap();
        assert!((uptime.value - 12345.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_redis_info_empty() {
        let records = parse_redis_info("", "svc");
        assert!(records.is_empty());
    }

    #[test]
    fn parse_redis_info_partial() {
        // Only memory section
        let info = "# Memory\nused_memory:512\n";
        let records = parse_redis_info(info, "svc");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "redis.memory.used");
    }

    #[test]
    fn scraper_config_creation() {
        let config = ScraperConfig {
            scrape_type: Some("postgres".into()),
            scrape_url: None,
            postgres_url: Some("postgres://localhost/test".into()),
            redis_url: None,
            tls_insecure: false,
            service_name: "test".into(),
        };
        assert_eq!(config.scrape_type, Some("postgres".into()));
    }
}
