use sqlx::PgPool;
use uuid::Uuid;

pub struct AuditEntry<'a> {
    pub actor_id: Uuid,
    pub actor_name: &'a str,
    pub action: &'a str,
    pub resource: &'a str,
    pub resource_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub detail: Option<serde_json::Value>,
    pub ip_addr: Option<&'a str>,
}

pub async fn write_audit(pool: &PgPool, entry: &AuditEntry<'_>) {
    if let Err(e) = write_audit_inner(pool, entry).await {
        tracing::warn!(
            error = %e,
            action = entry.action,
            resource = entry.resource,
            "failed to write audit log entry"
        );
    }
}

async fn write_audit_inner(pool: &PgPool, entry: &AuditEntry<'_>) -> Result<(), sqlx::Error> {
    let ip: Option<ipnetwork::IpNetwork> = entry.ip_addr.and_then(|s| s.parse().ok());

    sqlx::query!(
        r#"
        INSERT INTO audit_log (actor_id, actor_name, action, resource, resource_id, project_id, detail, ip_addr)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
        entry.actor_id,
        entry.actor_name,
        entry.action,
        entry.resource,
        entry.resource_id,
        entry.project_id,
        entry.detail,
        ip as Option<ipnetwork::IpNetwork>,
    )
    .execute(pool)
    .await?;

    Ok(())
}
