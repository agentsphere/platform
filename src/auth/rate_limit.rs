use fred::interfaces::KeysInterface;

use crate::error::ApiError;

/// Sliding-window rate limiter backed by Valkey.
///
/// Increments a counter keyed on `rate:{prefix}:{identifier}` with a TTL of
/// `window_secs`. Returns `Err(ApiError::TooManyRequests)` when the counter
/// exceeds `max_attempts`.
pub async fn check_rate(
    valkey: &fred::clients::Pool,
    prefix: &str,
    identifier: &str,
    max_attempts: u64,
    window_secs: i64,
) -> Result<(), ApiError> {
    let key = format!("rate:{prefix}:{identifier}");

    let count: u64 = valkey.incr(&key).await.map_err(ApiError::from)?;

    // Set expiry only when the key is newly created (count == 1)
    if count == 1 {
        let _: () = valkey
            .expire(&key, window_secs, None)
            .await
            .map_err(ApiError::from)?;
    }

    if count > max_attempts {
        return Err(ApiError::TooManyRequests);
    }

    Ok(())
}
