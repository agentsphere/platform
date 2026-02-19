use fred::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[tracing::instrument(skip(url), err)]
pub async fn connect(url: &str) -> anyhow::Result<fred::clients::Pool> {
    let config = fred::types::config::Config::from_url(url)?;
    let pool = fred::clients::Pool::new(config, None, None, None, 4)?;
    pool.init().await?;

    tracing::info!("connected to valkey");
    Ok(pool)
}

#[allow(dead_code)]
pub async fn get_cached<T: DeserializeOwned>(pool: &fred::clients::Pool, key: &str) -> Option<T> {
    let value: Option<String> = pool.get(key).await.ok()?;
    value.and_then(|v| serde_json::from_str(&v).ok())
}

#[allow(dead_code)]
pub async fn set_cached<T: Serialize>(
    pool: &fred::clients::Pool,
    key: &str,
    value: &T,
    ttl_secs: i64,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(value)?;
    let expiration = Some(Expiration::EX(ttl_secs));
    pool.set::<(), _, _>(key, json, expiration, None, false)
        .await?;
    Ok(())
}

#[allow(dead_code)]
pub async fn invalidate(pool: &fred::clients::Pool, key: &str) -> anyhow::Result<()> {
    pool.del::<(), _>(key).await?;
    Ok(())
}

#[allow(dead_code)]
pub async fn publish(
    pool: &fred::clients::Pool,
    channel: &str,
    message: &str,
) -> anyhow::Result<()> {
    pool.next().publish::<(), _, _>(channel, message).await?;
    Ok(())
}
