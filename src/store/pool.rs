use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

#[tracing::instrument(skip(url), err)]
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?;

    tracing::info!("connected to postgres");

    sqlx::migrate!().run(&pool).await?;
    tracing::info!("migrations applied");

    Ok(pool)
}
