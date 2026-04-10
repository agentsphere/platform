-- #18: Add updated_at to mutable tables missing it.

ALTER TABLE webhooks ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TRIGGER trg_webhooks_updated_at
    BEFORE UPDATE ON webhooks
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

ALTER TABLE alert_rules ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TRIGGER trg_alert_rules_updated_at
    BEFORE UPDATE ON alert_rules
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

ALTER TABLE api_tokens ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TRIGGER trg_api_tokens_updated_at
    BEFORE UPDATE ON api_tokens
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
