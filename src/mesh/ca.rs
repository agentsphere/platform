//! Mesh CA: root certificate authority and leaf certificate issuance.

use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, Utc};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber,
};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::error::MeshError;
use super::identity::SpiffeId;
use crate::config::Config;
use crate::secrets::engine;

/// A certificate bundle returned after issuance.
#[derive(Debug, Serialize)]
pub struct CertBundle {
    pub cert_pem: String,
    pub key_pem: String,
    pub ca_pem: String,
    pub not_after: DateTime<Utc>,
}

/// The mesh certificate authority.
///
/// Holds the root CA key/cert in memory and issues leaf certificates
/// signed by the root CA.
pub struct MeshCa {
    ca_id: Uuid,
    root_key_pem: String,
    root_cert_pem: String,
    serial: AtomicI64,
    cert_ttl_secs: u64,
}

impl MeshCa {
    /// Initialize the mesh CA.
    ///
    /// On first boot: generates a self-signed P-256 root CA, encrypts the
    /// private key with the platform secrets engine, and stores it in the DB.
    /// On subsequent boots: loads from the DB and decrypts the key.
    #[tracing::instrument(skip(pool, config), err)]
    pub async fn init(pool: &PgPool, config: &Config) -> Result<Self, MeshError> {
        let master_key = resolve_master_key(config)?;
        let root_ttl_days = config.mesh_ca_root_ttl_days;
        let cert_ttl_secs = config.mesh_ca_cert_ttl_secs;

        // Check for existing CA in the DB
        let existing = sqlx::query!(
            r#"SELECT id, root_cert_pem, secret_name, serial_counter FROM mesh_ca ORDER BY created_at DESC LIMIT 1"#,
        )
        .fetch_optional(pool)
        .await?;

        if let Some(row) = existing {
            // Load and decrypt the root key from secrets table
            let root_key_pem = load_root_key(pool, &master_key, &row.secret_name).await?;

            tracing::info!(ca_id = %row.id, serial = row.serial_counter, "mesh CA loaded from DB");
            return Ok(Self {
                ca_id: row.id,
                root_key_pem,
                root_cert_pem: row.root_cert_pem,
                serial: AtomicI64::new(row.serial_counter),
                cert_ttl_secs,
            });
        }

        // First boot: generate new root CA
        let (root_cert_pem, root_key_pem) = generate_root_ca(root_ttl_days)?;

        // Encrypt and store root key
        let ca_id = Uuid::new_v4();
        let secret_name = format!("mesh-ca-root-key-{ca_id}");
        store_root_key(pool, &master_key, &secret_name, &root_key_pem).await?;

        // Store CA metadata
        let not_after_dt = Utc::now() + chrono::Duration::days(i64::from(root_ttl_days));
        sqlx::query!(
            r#"INSERT INTO mesh_ca (id, root_cert_pem, secret_name, serial_counter, not_after)
               VALUES ($1, $2, $3, 1, $4)"#,
            ca_id,
            root_cert_pem,
            secret_name,
            not_after_dt,
        )
        .execute(pool)
        .await?;

        tracing::info!(%ca_id, "mesh CA created");
        Ok(Self {
            ca_id,
            root_key_pem,
            root_cert_pem,
            serial: AtomicI64::new(1),
            cert_ttl_secs,
        })
    }

    /// Issue a leaf certificate for the given SPIFFE identity.
    #[tracing::instrument(skip(self, pool), fields(%spiffe_id, %namespace, %service), err)]
    pub async fn issue_cert(
        &self,
        pool: &PgPool,
        spiffe_id: &SpiffeId,
        namespace: &str,
        service: &str,
    ) -> Result<CertBundle, MeshError> {
        // Increment serial atomically and persist to DB
        let serial = self.serial.fetch_add(1, Ordering::SeqCst) + 1;
        persist_serial(pool, self.ca_id, serial).await?;

        // Parse root CA key and cert for signing
        let root_key = KeyPair::from_pem(&self.root_key_pem)
            .map_err(|e| MeshError::CertGeneration(format!("parse root key: {e}")))?;
        let root_params = CertificateParams::from_ca_cert_pem(&self.root_cert_pem)
            .map_err(|e| MeshError::CertGeneration(format!("parse root cert: {e}")))?;
        let root_cert = root_params
            .self_signed(&root_key)
            .map_err(|e| MeshError::CertGeneration(format!("reconstruct root cert: {e}")))?;

        // Generate leaf key pair
        let leaf_key = KeyPair::generate()
            .map_err(|e| MeshError::CertGeneration(format!("generate leaf key: {e}")))?;

        // Build leaf certificate parameters
        let now = OffsetDateTime::now_utc();
        let not_after = now + time::Duration::seconds(self.cert_ttl_secs.cast_signed());

        let leaf_cert = build_leaf_params(spiffe_id, service, namespace, serial, now, not_after)?
            .signed_by(&leaf_key, &root_cert, &root_key)
            .map_err(|e| MeshError::CertGeneration(format!("sign leaf cert: {e}")))?;

        let not_after_chrono = offset_to_chrono(not_after);
        let not_before_chrono = offset_to_chrono(now);

        // Record issuance in audit table
        sqlx::query!(
            r#"INSERT INTO mesh_certs (ca_id, spiffe_id, serial, not_before, not_after, namespace, service)
               VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
            self.ca_id,
            spiffe_id.uri(),
            serial,
            not_before_chrono,
            not_after_chrono,
            namespace,
            service,
        )
        .execute(pool)
        .await?;

        Ok(CertBundle {
            cert_pem: leaf_cert.pem(),
            key_pem: leaf_key.serialize_pem(),
            ca_pem: self.root_cert_pem.clone(),
            not_after: not_after_chrono,
        })
    }

    /// Return the root CA certificate PEM (trust bundle).
    pub fn trust_bundle(&self) -> &str {
        &self.root_cert_pem
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_master_key(config: &Config) -> Result<[u8; 32], MeshError> {
    let key_hex = config
        .master_key
        .as_deref()
        .ok_or_else(|| MeshError::CaInit("PLATFORM_MASTER_KEY required for mesh CA".into()))?;
    engine::parse_master_key(key_hex)
        .map_err(|e| MeshError::CaInit(format!("invalid master key: {e}")))
}

async fn load_root_key(
    pool: &PgPool,
    master_key: &[u8; 32],
    secret_name: &str,
) -> Result<String, MeshError> {
    let row = sqlx::query!(
        "SELECT encrypted_value FROM secrets WHERE name = $1 AND project_id IS NULL",
        secret_name,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| MeshError::CaInit(format!("root key secret {secret_name} not found")))?;

    let plaintext = engine::decrypt(&row.encrypted_value, master_key, None)
        .map_err(|e| MeshError::CaInit(format!("decrypt root key: {e}")))?;

    String::from_utf8(plaintext)
        .map_err(|e| MeshError::CaInit(format!("root key is not valid UTF-8: {e}")))
}

async fn store_root_key(
    pool: &PgPool,
    master_key: &[u8; 32],
    secret_name: &str,
    root_key_pem: &str,
) -> Result<(), MeshError> {
    let encrypted = engine::encrypt(root_key_pem.as_bytes(), master_key)
        .map_err(|e| MeshError::CaInit(format!("encrypt root key: {e}")))?;

    sqlx::query!(
        r#"INSERT INTO secrets (name, encrypted_value, scope, created_by)
           VALUES ($1, $2, 'all', (SELECT id FROM users WHERE name = 'admin' LIMIT 1))"#,
        secret_name,
        encrypted,
    )
    .execute(pool)
    .await?;

    Ok(())
}

fn generate_root_ca(root_ttl_days: u32) -> Result<(String, String), MeshError> {
    let key_pair = KeyPair::generate()
        .map_err(|e| MeshError::CertGeneration(format!("generate root key: {e}")))?;

    let now = OffsetDateTime::now_utc();
    let not_after = now + time::Duration::days(i64::from(root_ttl_days));

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Platform Mesh CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = now;
    params.not_after = not_after;
    params.serial_number = Some(SerialNumber::from(1_u64));

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| MeshError::CertGeneration(format!("self-sign root CA: {e}")))?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

fn build_leaf_params(
    spiffe_id: &SpiffeId,
    service: &str,
    namespace: &str,
    serial: i64,
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
) -> Result<CertificateParams, MeshError> {
    let spiffe_uri: rcgen::Ia5String = spiffe_id
        .uri()
        .as_str()
        .try_into()
        .map_err(|e| MeshError::CertGeneration(format!("invalid SPIFFE URI: {e}")))?;

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("{service}.{namespace}"));
    params.is_ca = IsCa::NoCa;
    params.subject_alt_names = vec![SanType::URI(spiffe_uri)];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.not_before = not_before;
    params.not_after = not_after;
    params.serial_number = Some(SerialNumber::from(serial.cast_unsigned()));

    Ok(params)
}

async fn persist_serial(pool: &PgPool, ca_id: Uuid, serial: i64) -> Result<(), MeshError> {
    sqlx::query!(
        "UPDATE mesh_ca SET serial_counter = $1 WHERE id = $2",
        serial,
        ca_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Convert `time::OffsetDateTime` to `chrono::DateTime<Utc>`.
fn offset_to_chrono(t: OffsetDateTime) -> DateTime<Utc> {
    DateTime::from_timestamp(t.unix_timestamp(), t.nanosecond()).unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_root_ca_produces_valid_pem() {
        let (cert_pem, key_pem) = generate_root_ca(365).unwrap();
        assert!(cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    }

    #[test]
    fn root_ca_can_sign_leaf() {
        let (cert_pem, key_pem) = generate_root_ca(365).unwrap();

        let root_key = KeyPair::from_pem(&key_pem).unwrap();
        let root_params = CertificateParams::from_ca_cert_pem(&cert_pem).unwrap();
        let root_cert = root_params.self_signed(&root_key).unwrap();

        let leaf_key = KeyPair::generate().unwrap();
        let spiffe_id = SpiffeId::new("default", "my-svc").unwrap();
        let now = OffsetDateTime::now_utc();
        let not_after = now + time::Duration::hours(1);

        let leaf_params =
            build_leaf_params(&spiffe_id, "my-svc", "default", 2, now, not_after).unwrap();
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &root_cert, &root_key)
            .unwrap();

        let leaf_pem = leaf_cert.pem();
        assert!(leaf_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn leaf_cert_has_spiffe_san() {
        let (cert_pem, key_pem) = generate_root_ca(365).unwrap();

        let root_key = KeyPair::from_pem(&key_pem).unwrap();
        let root_params = CertificateParams::from_ca_cert_pem(&cert_pem).unwrap();
        let root_cert = root_params.self_signed(&root_key).unwrap();

        let leaf_key = KeyPair::generate().unwrap();
        let spiffe_id = SpiffeId::new("prod", "api").unwrap();
        let now = OffsetDateTime::now_utc();
        let not_after = now + time::Duration::hours(1);

        let leaf_params = build_leaf_params(&spiffe_id, "api", "prod", 3, now, not_after).unwrap();
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &root_cert, &root_key)
            .unwrap();

        // Parse the DER to verify SAN
        let der = leaf_cert.der();
        let (_, x509) = x509_parser::parse_x509_certificate(der).unwrap();

        // Check the SAN extension contains our SPIFFE URI
        let san = x509.subject_alternative_name().unwrap().unwrap();
        let uri_found = san.value.general_names.iter().any(|gn| {
            matches!(gn, x509_parser::extensions::GeneralName::URI(uri) if *uri == "spiffe://platform/prod/api")
        });
        assert!(uri_found, "leaf cert must contain SPIFFE URI SAN");
    }

    #[test]
    fn offset_to_chrono_roundtrip() {
        let now = OffsetDateTime::now_utc();
        let chrono_dt = offset_to_chrono(now);
        // Should be within 1 second
        let diff = (chrono_dt.timestamp() - now.unix_timestamp()).abs();
        assert!(diff <= 1, "timestamps should be within 1 second");
    }
}
