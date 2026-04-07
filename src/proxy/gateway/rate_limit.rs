//! Token bucket rate limiter for gateway routes.
//!
//! Per-route rate limiting using token bucket algorithm. Configuration is read
//! from `HTTPRoute` annotations: `platform.io/rate-limit`, `platform.io/rate-limit-window`,
//! `platform.io/rate-limit-burst`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::watch;

/// Rate limit configuration for a single route, parsed from annotations.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum requests per window.
    pub limit: u64,
    /// Window duration in seconds.
    pub window_secs: u64,
    /// Burst allowance (extra tokens above the steady-state refill).
    pub burst: u64,
}

impl RateLimitConfig {
    /// Token refill rate: tokens per second.
    fn refill_rate(&self) -> f64 {
        if self.window_secs == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let rate = self.limit as f64 / self.window_secs as f64;
        rate
    }

    /// Maximum bucket capacity (`limit` + `burst`).
    fn capacity(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let cap = self.limit as f64 + self.burst as f64;
        cap
    }
}

/// Key for rate limit buckets: `(route_name, client_ip)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateLimitKey {
    pub route_name: String,
    pub client_ip: String,
}

impl RateLimitKey {
    /// Construct a rate limit key from route name and client IP.
    pub fn new(route_name: &str, client_ip: &str) -> Self {
        Self {
            route_name: route_name.to_string(),
            client_ip: client_ip.to_string(),
        }
    }
}

/// A token bucket tracking available tokens and last refill time.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    last_access: Instant,
}

impl TokenBucket {
    fn new(capacity: f64) -> Self {
        let now = Instant::now();
        Self {
            tokens: capacity,
            last_refill: now,
            last_access: now,
        }
    }
}

/// Concurrent rate limiter using token bucket algorithm.
///
/// Each `(route, client_ip)` pair gets its own bucket. Buckets are lazily created
/// on first access and evicted by a background cleanup task.
pub struct RateLimiter {
    buckets: DashMap<RateLimitKey, TokenBucket>,
}

impl RateLimiter {
    /// Create a new empty rate limiter.
    pub fn new() -> Self {
        Self {
            buckets: DashMap::new(),
        }
    }

    /// Check if a request is allowed under the rate limit.
    ///
    /// Returns `true` if allowed (token consumed), `false` if rate-limited (bucket empty).
    pub fn check(&self, key: &RateLimitKey, config: &RateLimitConfig) -> bool {
        let capacity = config.capacity();
        let refill_rate = config.refill_rate();

        let mut bucket = self
            .buckets
            .entry(key.clone())
            .or_insert_with(|| TokenBucket::new(capacity));

        let now = Instant::now();
        bucket.last_access = now;

        // Refill tokens based on elapsed time
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_rate).min(capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Remove stale entries not accessed within `max_idle` duration.
    pub fn evict_stale(&self, max_idle: Duration) {
        let now = Instant::now();
        self.buckets
            .retain(|_, bucket| now.duration_since(bucket.last_access) < max_idle);
    }

    /// Number of active buckets (for diagnostics).
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether there are no active buckets.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Background cleanup task that evicts stale rate limit buckets every 60 seconds.
/// Entries not accessed for `2 * window_secs` (minimum 120s) are removed.
#[tracing::instrument(skip_all)]
pub async fn run_cleanup(
    limiter: Arc<RateLimiter>,
    eviction_idle: Duration,
    mut shutdown: watch::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let before = limiter.len();
                limiter.evict_stale(eviction_idle);
                let removed = before.saturating_sub(limiter.len());
                if removed > 0 {
                    tracing::debug!(removed, remaining = limiter.len(), "rate limit cleanup");
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("rate limit cleanup task exiting");
}

/// Parse rate limit configuration from `HTTPRoute` annotations.
///
/// Looks for:
/// - `platform.io/rate-limit`: requests per window (required, must be > 0)
/// - `platform.io/rate-limit-window`: window in seconds (default: `60`)
/// - `platform.io/rate-limit-burst`: burst allowance (default: `20`)
///
/// Returns `None` if `platform.io/rate-limit` is not set or not a valid number.
pub fn parse_annotations(
    annotations: &std::collections::BTreeMap<String, String>,
) -> Option<RateLimitConfig> {
    let limit_str = annotations.get("platform.io/rate-limit")?;
    let limit: u64 = limit_str.parse().ok().filter(|&v| v > 0)?;

    let window_secs = annotations
        .get("platform.io/rate-limit-window")
        .and_then(|s| s.parse().ok())
        .unwrap_or(60u64);

    let burst = annotations
        .get("platform.io/rate-limit-burst")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20u64);

    Some(RateLimitConfig {
        limit,
        window_secs,
        burst,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn token_bucket_allows_within_limit() {
        let limiter = RateLimiter::new();
        let key = RateLimitKey::new("my-route", "192.168.1.1");
        let config = RateLimitConfig {
            limit: 10,
            window_secs: 60,
            burst: 0,
        };

        // Should allow 10 requests (initial tokens = limit + burst = 10)
        for i in 0..10 {
            assert!(
                limiter.check(&key, &config),
                "request {i} should be allowed"
            );
        }
    }

    #[test]
    fn token_bucket_rejects_after_exhaustion() {
        let limiter = RateLimiter::new();
        let key = RateLimitKey::new("my-route", "10.0.0.1");
        let config = RateLimitConfig {
            limit: 5,
            window_secs: 60,
            burst: 0,
        };

        // Exhaust all tokens
        for _ in 0..5 {
            assert!(limiter.check(&key, &config));
        }

        // Next request should be rejected
        assert!(!limiter.check(&key, &config));
        assert!(!limiter.check(&key, &config));
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let limiter = RateLimiter::new();
        let key = RateLimitKey::new("refill-route", "10.0.0.2");
        let config = RateLimitConfig {
            limit: 10,
            window_secs: 10,
            burst: 0,
        };

        // Exhaust all tokens
        for _ in 0..10 {
            assert!(limiter.check(&key, &config));
        }
        assert!(!limiter.check(&key, &config));

        // Manually advance the bucket's last_refill to simulate time passing
        // refill_rate = 10/10 = 1 token/sec, so 2 seconds = 2 tokens
        {
            let mut bucket = limiter.buckets.get_mut(&key).expect("bucket should exist");
            bucket.last_refill -= Duration::from_secs(2);
        }

        // Should have ~2 tokens now
        assert!(limiter.check(&key, &config));
        assert!(limiter.check(&key, &config));
        assert!(!limiter.check(&key, &config));
    }

    #[test]
    fn token_bucket_burst_allows_spike() {
        let limiter = RateLimiter::new();
        let key = RateLimitKey::new("burst-route", "10.0.0.3");
        let config = RateLimitConfig {
            limit: 5,
            window_secs: 60,
            burst: 10,
        };

        // Capacity = limit + burst = 15
        for i in 0..15 {
            assert!(
                limiter.check(&key, &config),
                "burst request {i} should be allowed"
            );
        }

        // 16th request should be rejected
        assert!(!limiter.check(&key, &config));
    }

    #[test]
    fn rate_limit_key_construction() {
        let key = RateLimitKey::new("my-route", "192.168.1.100");
        assert_eq!(key.route_name, "my-route");
        assert_eq!(key.client_ip, "192.168.1.100");

        // Different IPs produce different keys
        let key2 = RateLimitKey::new("my-route", "192.168.1.200");
        assert_ne!(key, key2);

        // Different routes produce different keys
        let key3 = RateLimitKey::new("other-route", "192.168.1.100");
        assert_ne!(key, key3);
    }

    #[test]
    fn eviction_removes_stale_entries() {
        let limiter = RateLimiter::new();
        let config = RateLimitConfig {
            limit: 10,
            window_secs: 60,
            burst: 0,
        };

        let key1 = RateLimitKey::new("route-a", "10.0.0.1");
        let key2 = RateLimitKey::new("route-b", "10.0.0.2");
        limiter.check(&key1, &config);
        limiter.check(&key2, &config);

        assert_eq!(limiter.len(), 2);

        // Make key1's bucket stale by rewinding its last_access
        {
            let mut bucket = limiter.buckets.get_mut(&key1).expect("bucket exists");
            bucket.last_access -= Duration::from_secs(300);
        }

        limiter.evict_stale(Duration::from_secs(120));
        assert_eq!(limiter.len(), 1);
        assert!(limiter.buckets.contains_key(&key2));
        assert!(!limiter.buckets.contains_key(&key1));
    }

    #[test]
    fn parse_annotations_full() {
        let mut annotations = BTreeMap::new();
        annotations.insert("platform.io/rate-limit".into(), "100".into());
        annotations.insert("platform.io/rate-limit-window".into(), "60".into());
        annotations.insert("platform.io/rate-limit-burst".into(), "20".into());

        let config = parse_annotations(&annotations).expect("should parse");
        assert_eq!(config.limit, 100);
        assert_eq!(config.window_secs, 60);
        assert_eq!(config.burst, 20);
    }

    #[test]
    fn parse_annotations_defaults() {
        let mut annotations = BTreeMap::new();
        annotations.insert("platform.io/rate-limit".into(), "50".into());

        let config = parse_annotations(&annotations).expect("should parse");
        assert_eq!(config.limit, 50);
        assert_eq!(config.window_secs, 60); // default
        assert_eq!(config.burst, 20); // default
    }

    #[test]
    fn parse_annotations_missing_rate_limit() {
        let annotations = BTreeMap::new();
        assert!(parse_annotations(&annotations).is_none());
    }

    #[test]
    fn parse_annotations_invalid_rate_limit() {
        let mut annotations = BTreeMap::new();
        annotations.insert("platform.io/rate-limit".into(), "not-a-number".into());
        assert!(parse_annotations(&annotations).is_none());
    }

    #[test]
    fn parse_annotations_zero_rate_limit() {
        let mut annotations = BTreeMap::new();
        annotations.insert("platform.io/rate-limit".into(), "0".into());
        assert!(parse_annotations(&annotations).is_none());
    }

    #[test]
    fn separate_buckets_per_key() {
        let limiter = RateLimiter::new();
        let config = RateLimitConfig {
            limit: 2,
            window_secs: 60,
            burst: 0,
        };

        let key_a = RateLimitKey::new("route", "10.0.0.1");
        let key_b = RateLimitKey::new("route", "10.0.0.2");

        // Exhaust key_a
        assert!(limiter.check(&key_a, &config));
        assert!(limiter.check(&key_a, &config));
        assert!(!limiter.check(&key_a, &config));

        // key_b should still have tokens
        assert!(limiter.check(&key_b, &config));
        assert!(limiter.check(&key_b, &config));
        assert!(!limiter.check(&key_b, &config));
    }

    #[test]
    fn refill_rate_calculation() {
        let config = RateLimitConfig {
            limit: 100,
            window_secs: 10,
            burst: 0,
        };
        assert!((config.refill_rate() - 10.0).abs() < f64::EPSILON);

        let config_zero = RateLimitConfig {
            limit: 100,
            window_secs: 0,
            burst: 0,
        };
        assert!((config_zero.refill_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn capacity_includes_burst() {
        let config = RateLimitConfig {
            limit: 100,
            window_secs: 60,
            burst: 50,
        };
        assert!((config.capacity() - 150.0).abs() < f64::EPSILON);
    }
}
