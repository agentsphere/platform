use sqlx::PgPool;
use uuid::Uuid;

#[allow(dead_code)] // ip_addr stored for future ipnetwork support
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
    // Note: ip_addr is INET in postgres; we skip binding it to avoid needing the
    // ipnetwork crate. The column stays NULL. A future pass can add ipnetwork to Cargo.toml.
    let _ = sqlx::query!(
        r#"
        INSERT INTO audit_log (actor_id, actor_name, action, resource, resource_id, project_id, detail)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        entry.actor_id,
        entry.actor_name,
        entry.action,
        entry.resource,
        entry.resource_id,
        entry.project_id,
        entry.detail,
    )
    .execute(pool)
    .await;
}
