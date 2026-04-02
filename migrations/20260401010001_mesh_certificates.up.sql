-- Mesh CA: root CA metadata (single row per CA generation)
CREATE TABLE mesh_ca (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    root_cert_pem   TEXT NOT NULL,
    secret_name     TEXT NOT NULL,
    serial_counter  BIGINT NOT NULL DEFAULT 1,
    not_after       TIMESTAMPTZ NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Mesh certificates: issued leaf certs for audit trail
CREATE TABLE mesh_certs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    ca_id           UUID NOT NULL REFERENCES mesh_ca(id) ON DELETE CASCADE,
    spiffe_id       TEXT NOT NULL,
    serial          BIGINT NOT NULL,
    not_before      TIMESTAMPTZ NOT NULL,
    not_after       TIMESTAMPTZ NOT NULL,
    namespace       TEXT NOT NULL,
    service         TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_mesh_certs_ca_id ON mesh_certs(ca_id);
CREATE INDEX idx_mesh_certs_spiffe_id ON mesh_certs(spiffe_id);
